// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use async_recursion::async_recursion;
use common_ast::ast::BinaryOperator;
use common_ast::ast::Expr;
use common_ast::ast::Expr::Array;
use common_ast::ast::GroupBy;
use common_ast::ast::Identifier;
use common_ast::ast::Join;
use common_ast::ast::JoinCondition;
use common_ast::ast::JoinOperator;
use common_ast::ast::Literal;
use common_ast::ast::OrderByExpr;
use common_ast::ast::Query;
use common_ast::ast::SelectStmt;
use common_ast::ast::SelectTarget;
use common_ast::ast::SetExpr;
use common_ast::ast::SetOperator;
use common_ast::ast::TableReference;
use common_exception::ErrorCode;
use common_exception::Result;
use common_exception::Span;
use common_expression::type_check::common_super_type;
use common_expression::types::DataType;
use common_functions::BUILTIN_FUNCTIONS;

use crate::binder::join::JoinConditions;
use crate::binder::project_set::SrfCollector;
use crate::binder::scalar_common::split_conjunctions;
use crate::binder::CteInfo;
use crate::binder::ExprContext;
use crate::binder::Visibility;
use crate::optimizer::SExpr;
use crate::planner::binder::scalar::ScalarBinder;
use crate::planner::binder::BindContext;
use crate::planner::binder::Binder;
use crate::plans::BoundColumnRef;
use crate::plans::CastExpr;
use crate::plans::EvalScalar;
use crate::plans::Filter;
use crate::plans::JoinType;
use crate::plans::ScalarExpr;
use crate::plans::ScalarItem;
use crate::plans::UnionAll;
use crate::ColumnBinding;
use crate::IndexType;

// A normalized IR for `SELECT` clause.
#[derive(Debug, Default)]
pub struct SelectList<'a> {
    pub items: Vec<SelectItem<'a>>,
}

#[derive(Debug)]
pub struct SelectItem<'a> {
    pub select_target: &'a SelectTarget,
    pub scalar: ScalarExpr,
    pub alias: String,
}

impl Binder {
    #[async_backtrace::framed]
    pub(super) async fn bind_select_stmt(
        &mut self,
        bind_context: &mut BindContext,
        stmt: &SelectStmt,
        order_by: &[OrderByExpr],
    ) -> Result<(SExpr, BindContext)> {
        let (mut s_expr, mut from_context) = if stmt.from.is_empty() {
            self.bind_one_table(bind_context, stmt).await?
        } else {
            let cross_joins = stmt
                .from
                .iter()
                .cloned()
                .reduce(|left, right| TableReference::Join {
                    span: None,
                    join: Join {
                        op: JoinOperator::CrossJoin,
                        condition: JoinCondition::None,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                })
                .unwrap();
            self.bind_table_reference(bind_context, &cross_joins)
                .await?
        };

        let mut rewriter = SelectRewriter::new(
            from_context.all_column_bindings(),
            self.name_resolution_ctx.unquoted_ident_case_sensitive,
        );
        let new_stmt = rewriter.rewrite(stmt)?;
        let stmt = new_stmt.as_ref().unwrap_or(stmt);

        if let Some(expr) = &stmt.selection {
            s_expr = self.bind_where(&mut from_context, expr, s_expr).await?;
        }

        // Collect set returning functions
        let set_returning_functions = {
            let mut collector = SrfCollector::new();
            stmt.select_list.iter().for_each(|item| {
                if let SelectTarget::AliasedExpr { expr, .. } = item {
                    collector.visit(expr);
                }
            });
            collector.into_srfs()
        };

        // Bind set returning functions
        s_expr = self
            .bind_project_set(&mut from_context, &set_returning_functions, s_expr)
            .await?;

        // Generate a analyzed select list with from context
        let mut select_list = self
            .normalize_select_list(&mut from_context, &stmt.select_list)
            .await?;

        // This will potentially add some alias group items to `from_context` if find some.
        if let Some(group_by) = stmt.group_by.as_ref() {
            self.analyze_group_items(&mut from_context, &select_list, group_by)
                .await?;
        }

        self.analyze_aggregate_select(&mut from_context, &mut select_list)?;

        // `analyze_window` should behind `analyze_aggregate_select`,
        // because `analyze_window` will rewrite the aggregate functions in the window function's arguments.
        self.analyze_window(&mut from_context, &mut select_list)?;

        // `analyze_projection` should behind `analyze_aggregate_select` because `analyze_aggregate_select` will rewrite `grouping`.
        let (mut scalar_items, projections) = self.analyze_projection(&select_list)?;

        let having = if let Some(having) = &stmt.having {
            Some(
                self.analyze_aggregate_having(&mut from_context, &select_list, having)
                    .await?,
            )
        } else {
            None
        };

        let order_items = self
            .analyze_order_items(
                &from_context,
                &mut scalar_items,
                &projections,
                order_by,
                stmt.distinct,
            )
            .await?;

        if !from_context.aggregate_info.aggregate_functions.is_empty()
            || !from_context.aggregate_info.group_items.is_empty()
        {
            s_expr = self.bind_aggregate(&mut from_context, s_expr).await?;
        }

        if let Some((having, span)) = having {
            s_expr = self
                .bind_having(&mut from_context, having, span, s_expr)
                .await?;
        }

        // bind window
        // window run after the HAVING clause but before the ORDER BY clause.
        for window_info in &from_context.windows.window_functions {
            s_expr = self.bind_window_function(window_info, s_expr).await?;
        }

        if stmt.distinct {
            s_expr = self.bind_distinct(
                stmt.span,
                &from_context,
                &projections,
                &mut scalar_items,
                s_expr,
            )?;
        }

        if !order_by.is_empty() {
            s_expr = self
                .bind_order_by(
                    &from_context,
                    order_items,
                    &select_list,
                    &mut scalar_items,
                    s_expr,
                )
                .await?;
        }

        s_expr = self.bind_projection(&mut from_context, &projections, &scalar_items, s_expr)?;

        // add internal column binding into expr
        s_expr = from_context.add_internal_column_into_expr(s_expr);

        let mut output_context = BindContext::new();
        output_context.parent = from_context.parent;
        output_context.columns = from_context.columns;
        output_context.ctes_map = from_context.ctes_map;

        Ok((s_expr, output_context))
    }

    #[async_recursion]
    #[async_backtrace::framed]
    pub(crate) async fn bind_set_expr(
        &mut self,
        bind_context: &mut BindContext,
        set_expr: &SetExpr,
        order_by: &[OrderByExpr],
    ) -> Result<(SExpr, BindContext)> {
        match set_expr {
            SetExpr::Select(stmt) => self.bind_select_stmt(bind_context, stmt, order_by).await,
            SetExpr::Query(stmt) => self.bind_query(bind_context, stmt).await,
            SetExpr::SetOperation(set_operation) => {
                self.bind_set_operator(
                    bind_context,
                    &set_operation.left,
                    &set_operation.right,
                    &set_operation.op,
                    &set_operation.all,
                )
                .await
            }
        }
    }

    #[async_recursion]
    #[async_backtrace::framed]
    pub(crate) async fn bind_query(
        &mut self,
        bind_context: &mut BindContext,
        query: &Query,
    ) -> Result<(SExpr, BindContext)> {
        if let Some(with) = &query.with {
            for cte in with.ctes.iter() {
                let table_name = cte.alias.name.name.clone();
                if bind_context.ctes_map.contains_key(&table_name) {
                    return Err(ErrorCode::SemanticError(format!(
                        "duplicate cte {table_name}"
                    )));
                }
                let cte_info = CteInfo {
                    columns_alias: cte.alias.columns.iter().map(|c| c.name.clone()).collect(),
                    query: cte.query.clone(),
                };
                bind_context.ctes_map.insert(table_name, cte_info);
            }
        }
        let (mut s_expr, bind_context) = match query.body {
            SetExpr::Select(_) | SetExpr::Query(_) => {
                self.bind_set_expr(bind_context, &query.body, &query.order_by)
                    .await?
            }
            SetExpr::SetOperation(_) => {
                let (mut s_expr, mut bind_context) =
                    self.bind_set_expr(bind_context, &query.body, &[]).await?;
                if !query.order_by.is_empty() {
                    s_expr = self
                        .bind_order_by_for_set_operation(&mut bind_context, s_expr, &query.order_by)
                        .await?;
                }
                (s_expr, bind_context)
            }
        };

        if !query.limit.is_empty() {
            if query.limit.len() == 1 {
                s_expr = self
                    .bind_limit(&bind_context, s_expr, Some(&query.limit[0]), &query.offset)
                    .await?;
            } else {
                s_expr = self
                    .bind_limit(
                        &bind_context,
                        s_expr,
                        Some(&query.limit[1]),
                        &Some(query.limit[0].clone()),
                    )
                    .await?;
            }
        } else if query.offset.is_some() {
            s_expr = self
                .bind_limit(&bind_context, s_expr, None, &query.offset)
                .await?;
        }

        Ok((s_expr, bind_context))
    }

    #[async_backtrace::framed]
    pub(super) async fn bind_where(
        &mut self,
        bind_context: &mut BindContext,
        expr: &Expr,
        child: SExpr,
    ) -> Result<SExpr> {
        bind_context.set_expr_context(ExprContext::WhereClause);

        let mut scalar_binder = ScalarBinder::new(
            bind_context,
            self.ctx.clone(),
            &self.name_resolution_ctx,
            self.metadata.clone(),
            &[],
        );
        let (scalar, _) = scalar_binder.bind(expr).await?;
        // if `Expr` is internal column, then add this internal column into `BindContext`
        if let ScalarExpr::BoundInternalColumnRef(ref internal_column) = scalar {
            bind_context
                .add_internal_column_binding(&internal_column.column, self.metadata.clone());
        };

        let filter_plan = Filter {
            predicates: split_conjunctions(&scalar),
            is_having: false,
        };
        let new_expr = SExpr::create_unary(filter_plan.into(), child);
        Ok(new_expr)
    }

    #[async_backtrace::framed]
    pub(super) async fn bind_set_operator(
        &mut self,
        bind_context: &mut BindContext,
        left: &SetExpr,
        right: &SetExpr,
        op: &SetOperator,
        all: &bool,
    ) -> Result<(SExpr, BindContext)> {
        let (left_expr, left_bind_context) = self.bind_set_expr(bind_context, left, &[]).await?;
        let (right_expr, right_bind_context) = self.bind_set_expr(bind_context, right, &[]).await?;
        let mut coercion_types = Vec::with_capacity(left_bind_context.columns.len());
        if left_bind_context.columns.len() != right_bind_context.columns.len() {
            return Err(ErrorCode::SemanticError(
                "SetOperation must have the same number of columns",
            ));
        } else {
            for (left_col, right_col) in left_bind_context
                .columns
                .iter()
                .zip(right_bind_context.columns.iter())
            {
                if left_col.data_type != right_col.data_type {
                    if let Some(data_type) = common_super_type(
                        *left_col.data_type.clone(),
                        *right_col.data_type.clone(),
                        &BUILTIN_FUNCTIONS.default_cast_rules,
                    ) {
                        coercion_types.push(data_type);
                    } else {
                        return Err(ErrorCode::SemanticError(format!(
                            "SetOperation's types cannot be matched, left column {:?}, type: {:?}, right column {:?}, type: {:?}",
                            left_col.column_name,
                            left_col.data_type,
                            right_col.column_name,
                            right_col.data_type
                        )));
                    }
                } else {
                    coercion_types.push(*left_col.data_type.clone());
                }
            }
        }
        match (op, all) {
            (SetOperator::Intersect, false) => {
                // Transfer Intersect to Semi join
                self.bind_intersect(
                    left.span(),
                    right.span(),
                    left_bind_context,
                    right_bind_context,
                    left_expr,
                    right_expr,
                )
            }
            (SetOperator::Except, false) => {
                // Transfer Except to Anti join
                self.bind_except(
                    left.span(),
                    right.span(),
                    left_bind_context,
                    right_bind_context,
                    left_expr,
                    right_expr,
                )
            }
            (SetOperator::Union, true) => self.bind_union(
                left.span(),
                right.span(),
                left_bind_context,
                right_bind_context,
                coercion_types,
                left_expr,
                right_expr,
                false,
            ),
            (SetOperator::Union, false) => self.bind_union(
                left.span(),
                right.span(),
                left_bind_context,
                right_bind_context,
                coercion_types,
                left_expr,
                right_expr,
                true,
            ),
            _ => Err(ErrorCode::Unimplemented(
                "Unsupported query type, currently, databend only support intersect distinct and except distinct",
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_union(
        &mut self,
        left_span: Span,
        right_span: Span,
        left_context: BindContext,
        right_context: BindContext,
        coercion_types: Vec<DataType>,
        left_expr: SExpr,
        right_expr: SExpr,
        distinct: bool,
    ) -> Result<(SExpr, BindContext)> {
        let (new_bind_context, pairs, left_expr, right_expr) = self.coercion_union_type(
            left_span,
            right_span,
            left_context,
            right_context,
            left_expr,
            right_expr,
            coercion_types,
        )?;

        let union_plan = UnionAll { pairs };
        let mut new_expr = SExpr::create_binary(union_plan.into(), left_expr, right_expr);
        if distinct {
            new_expr = self.bind_distinct(
                left_span,
                &new_bind_context,
                new_bind_context.all_column_bindings(),
                &mut HashMap::new(),
                new_expr,
            )?;
        }

        Ok((new_expr, new_bind_context))
    }

    fn bind_intersect(
        &mut self,
        left_span: Span,
        right_span: Span,
        left_context: BindContext,
        right_context: BindContext,
        left_expr: SExpr,
        right_expr: SExpr,
    ) -> Result<(SExpr, BindContext)> {
        self.bind_intersect_or_except(
            left_span,
            right_span,
            left_context,
            right_context,
            left_expr,
            right_expr,
            JoinType::LeftSemi,
        )
    }

    fn bind_except(
        &mut self,
        left_span: Span,
        right_span: Span,
        left_context: BindContext,
        right_context: BindContext,
        left_expr: SExpr,
        right_expr: SExpr,
    ) -> Result<(SExpr, BindContext)> {
        self.bind_intersect_or_except(
            left_span,
            right_span,
            left_context,
            right_context,
            left_expr,
            right_expr,
            JoinType::LeftAnti,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_intersect_or_except(
        &mut self,
        left_span: Span,
        right_span: Span,
        left_context: BindContext,
        right_context: BindContext,
        left_expr: SExpr,
        right_expr: SExpr,
        join_type: JoinType,
    ) -> Result<(SExpr, BindContext)> {
        let left_expr = self.bind_distinct(
            left_span,
            &left_context,
            left_context.all_column_bindings(),
            &mut HashMap::new(),
            left_expr,
        )?;
        let mut left_conditions = Vec::with_capacity(left_context.columns.len());
        let mut right_conditions = Vec::with_capacity(right_context.columns.len());
        assert_eq!(left_context.columns.len(), right_context.columns.len());
        for (left_column, right_column) in left_context
            .columns
            .iter()
            .zip(right_context.columns.iter())
        {
            left_conditions.push(
                BoundColumnRef {
                    span: left_span,
                    column: left_column.clone(),
                }
                .into(),
            );
            right_conditions.push(
                BoundColumnRef {
                    span: right_span,
                    column: right_column.clone(),
                }
                .into(),
            );
        }
        let join_conditions = JoinConditions {
            left_conditions,
            right_conditions,
            non_equi_conditions: vec![],
            other_conditions: vec![],
        };
        let s_expr = self.bind_join_with_type(join_type, join_conditions, left_expr, right_expr)?;
        Ok((s_expr, left_context))
    }

    #[allow(clippy::type_complexity)]
    #[allow(clippy::too_many_arguments)]
    fn coercion_union_type(
        &self,
        left_span: Span,
        right_span: Span,
        left_bind_context: BindContext,
        right_bind_context: BindContext,
        mut left_expr: SExpr,
        mut right_expr: SExpr,
        coercion_types: Vec<DataType>,
    ) -> Result<(BindContext, Vec<(IndexType, IndexType)>, SExpr, SExpr)> {
        let mut left_scalar_items = Vec::with_capacity(left_bind_context.columns.len());
        let mut right_scalar_items = Vec::with_capacity(right_bind_context.columns.len());
        let mut new_bind_context = BindContext::new();
        let mut pairs = Vec::with_capacity(left_bind_context.columns.len());
        for (idx, (left_col, right_col)) in left_bind_context
            .columns
            .iter()
            .zip(right_bind_context.columns.iter())
            .enumerate()
        {
            let left_index = if *left_col.data_type != coercion_types[idx] {
                let new_column_index = self
                    .metadata
                    .write()
                    .add_derived_column(left_col.column_name.clone(), coercion_types[idx].clone());
                let column_binding = ColumnBinding {
                    database_name: None,
                    table_name: None,
                    column_name: left_col.column_name.clone(),
                    index: new_column_index,
                    data_type: Box::new(coercion_types[idx].clone()),
                    visibility: Visibility::Visible,
                };
                let left_coercion_expr = CastExpr {
                    span: left_span,
                    is_try: false,
                    argument: Box::new(
                        BoundColumnRef {
                            span: left_span,
                            column: left_col.clone(),
                        }
                        .into(),
                    ),
                    target_type: Box::new(coercion_types[idx].clone()),
                };
                left_scalar_items.push(ScalarItem {
                    scalar: left_coercion_expr.into(),
                    index: new_column_index,
                });
                new_bind_context.add_column_binding(column_binding);
                new_column_index
            } else {
                new_bind_context.add_column_binding(left_col.clone());
                left_col.index
            };
            let right_index = if *right_col.data_type != coercion_types[idx] {
                let new_column_index = self
                    .metadata
                    .write()
                    .add_derived_column(right_col.column_name.clone(), coercion_types[idx].clone());
                let right_coercion_expr = CastExpr {
                    span: right_span,
                    is_try: false,
                    argument: Box::new(
                        BoundColumnRef {
                            span: right_span,
                            column: right_col.clone(),
                        }
                        .into(),
                    ),
                    target_type: Box::new(coercion_types[idx].clone()),
                };
                right_scalar_items.push(ScalarItem {
                    scalar: right_coercion_expr.into(),
                    index: new_column_index,
                });
                new_column_index
            } else {
                right_col.index
            };
            pairs.push((left_index, right_index));
        }
        if !left_scalar_items.is_empty() {
            left_expr = SExpr::create_unary(
                EvalScalar {
                    items: left_scalar_items,
                }
                .into(),
                left_expr,
            );
        }
        if !right_scalar_items.is_empty() {
            right_expr = SExpr::create_unary(
                EvalScalar {
                    items: right_scalar_items,
                }
                .into(),
                right_expr,
            );
        }
        Ok((new_bind_context, pairs, left_expr, right_expr))
    }
}

/// It is useful when implementing some SQL syntax sugar,
///
/// [`column_binding`] contains the column binding information of the SelectStmt.
///
/// to rewrite the SelectStmt, just add a new rewrite_* function and call it in the `rewrite` function.
struct SelectRewriter<'a> {
    column_binding: &'a [ColumnBinding],
    new_stmt: Option<SelectStmt>,
    is_unquoted_ident_case_sensitive: bool,
}

// helper functions to SelectRewriter
impl<'a> SelectRewriter<'a> {
    fn compare_unquoted_ident(&self, a: &str, b: &str) -> bool {
        if self.is_unquoted_ident_case_sensitive {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    }
    fn parse_aggregate_function(expr: &Expr) -> Result<(&Identifier, &[Expr])> {
        match expr {
            Expr::FunctionCall { name, args, .. } => Ok((name, args)),
            _ => Err(ErrorCode::SyntaxException("Aggregate function is required")),
        }
    }

    fn ident_from_string(s: &str) -> Identifier {
        Identifier {
            name: s.to_string(),
            quote: None,
            span: None,
        }
    }

    fn expr_eq_from_col_and_value(col: Identifier, value: Expr) -> Expr {
        Expr::BinaryOp {
            span: None,
            left: Box::new(Expr::ColumnRef {
                column: col,
                span: None,
                database: None,
                table: None,
            }),
            op: BinaryOperator::Eq,
            right: Box::new(value),
        }
    }

    fn target_func_from_name_args(
        name: Identifier,
        args: Vec<Expr>,
        alias: Option<Identifier>,
    ) -> SelectTarget {
        SelectTarget::AliasedExpr {
            expr: Box::new(Expr::FunctionCall {
                span: Span::default(),
                distinct: false,
                name,
                args,
                params: vec![],
                window: None,
            }),
            alias,
        }
    }

    fn expr_literal_array_from_vec_ident(exprs: Vec<Identifier>) -> Expr {
        Array {
            span: Span::default(),
            exprs: exprs
                .into_iter()
                .map(|expr| Expr::Literal {
                    span: None,
                    lit: Literal::String(expr.name),
                })
                .collect(),
        }
    }

    fn expr_column_ref_array_from_vec_ident(exprs: Vec<Identifier>) -> Expr {
        Array {
            span: Span::default(),
            exprs: exprs
                .into_iter()
                .map(|expr| Expr::ColumnRef {
                    span: None,
                    column: expr,
                    database: None,
                    table: None,
                })
                .collect(),
        }
    }

    // For Expr::Literal, expr.to_string() is quoted, sometimes we need the raw string.
    fn raw_string_from_literal_expr(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Literal { lit, .. } => match lit {
                Literal::String(v) => Some(v.clone()),
                _ => Some(expr.to_string()),
            },
            _ => None,
        }
    }
}

impl<'a> SelectRewriter<'a> {
    fn new(column_binding: &'a [ColumnBinding], is_unquoted_ident_case_sensitive: bool) -> Self {
        SelectRewriter {
            column_binding,
            new_stmt: None,
            is_unquoted_ident_case_sensitive,
        }
    }

    fn rewrite(&mut self, stmt: &SelectStmt) -> Result<Option<SelectStmt>> {
        self.rewrite_pivot(stmt)?;
        self.rewrite_unpivot(stmt)?;
        Ok(self.new_stmt.take())
    }
    fn rewrite_pivot(&mut self, stmt: &SelectStmt) -> Result<()> {
        if stmt.from.len() != 1 || stmt.from[0].pivot().is_none() {
            return Ok(());
        }
        let pivot = stmt.from[0].pivot().unwrap();
        let (aggregate_name, aggregate_args) = Self::parse_aggregate_function(&pivot.aggregate)?;
        let aggregate_columns = aggregate_args
            .iter()
            .map(|expr| match expr {
                Expr::ColumnRef { column, .. } => Some(column.clone()),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| ErrorCode::SyntaxException("Aggregate column not found"))?;
        let aggregate_column_names = aggregate_columns
            .iter()
            .map(|col| col.name.as_str())
            .collect::<Vec<_>>();
        let new_group_by = stmt.group_by.clone().unwrap_or_else(|| {
            GroupBy::Normal(
                self.column_binding
                    .iter()
                    .filter(|col_bind| {
                        !self
                            .compare_unquoted_ident(&col_bind.column_name, &pivot.value_column.name)
                            && !aggregate_column_names
                                .iter()
                                .any(|col| self.compare_unquoted_ident(col, &col_bind.column_name))
                    })
                    .map(|col| Expr::Literal {
                        span: Span::default(),
                        lit: Literal::UInt64(col.index as u64 + 1),
                    })
                    .collect(),
            )
        });

        let mut new_select_list = stmt.select_list.clone();
        if let Some(star) = new_select_list.iter_mut().find(|target| target.is_star()) {
            let mut exclude_columns = aggregate_columns;
            exclude_columns.push(pivot.value_column.clone());
            star.exclude(exclude_columns);
        };
        let new_aggregate_name = Identifier {
            name: format!("{}_if", aggregate_name.name),
            ..aggregate_name.clone()
        };
        for value in &pivot.values {
            let mut args = aggregate_args.to_vec();
            args.push(Self::expr_eq_from_col_and_value(
                pivot.value_column.clone(),
                value.clone(),
            ));
            let alias = Self::raw_string_from_literal_expr(value)
                .ok_or_else(|| ErrorCode::SyntaxException("Pivot value should be literal"))?;
            new_select_list.push(Self::target_func_from_name_args(
                new_aggregate_name.clone(),
                args,
                Some(Self::ident_from_string(&alias)),
            ));
        }

        if let Some(ref mut new_stmt) = self.new_stmt {
            new_stmt.select_list = new_select_list;
            new_stmt.group_by = Some(new_group_by);
        } else {
            self.new_stmt = Some(SelectStmt {
                select_list: new_select_list,
                group_by: Some(new_group_by),
                ..stmt.clone()
            });
        }
        Ok(())
    }

    fn rewrite_unpivot(&mut self, stmt: &SelectStmt) -> Result<()> {
        if stmt.from.len() != 1 || stmt.from[0].unpivot().is_none() {
            return Ok(());
        }
        let unpivot = stmt.from[0].unpivot().unwrap();
        let mut new_select_list = stmt.select_list.clone();
        if let Some(star) = new_select_list.iter_mut().find(|target| target.is_star()) {
            star.exclude(unpivot.names.clone());
        };
        new_select_list.push(Self::target_func_from_name_args(
            Self::ident_from_string("unnest"),
            vec![Self::expr_literal_array_from_vec_ident(
                unpivot.names.clone(),
            )],
            Some(unpivot.column_name.clone()),
        ));
        new_select_list.push(Self::target_func_from_name_args(
            Self::ident_from_string("unnest"),
            vec![Self::expr_column_ref_array_from_vec_ident(
                unpivot.names.clone(),
            )],
            Some(unpivot.value_column.clone()),
        ));

        if let Some(ref mut new_stmt) = self.new_stmt {
            new_stmt.select_list = new_select_list;
        } else {
            self.new_stmt = Some(SelectStmt {
                select_list: new_select_list,
                ..stmt.clone()
            });
        };
        Ok(())
    }
}
