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

use common_exception::Result;
use common_expression::Scalar;

use crate::optimizer::rule::Rule;
use crate::optimizer::RuleID;
use crate::optimizer::SExpr;
use crate::plans::ConstantExpr;
use crate::plans::Filter;
use crate::plans::PatternPlan;
use crate::plans::RelOp;
use crate::plans::ScalarExpr;

fn normalize_predicates(predicates: Vec<ScalarExpr>) -> Vec<ScalarExpr> {
    [remove_true_predicate, normalize_falsy_predicate]
        .into_iter()
        .fold(predicates, |acc, f| f(acc))
}

fn is_true(predicate: &ScalarExpr) -> bool {
    matches!(
        predicate,
        ScalarExpr::ConstantExpr(ConstantExpr {
            value: Scalar::Boolean(true),
            ..
        })
    )
}

fn is_falsy(predicate: &ScalarExpr) -> bool {
    matches!(
        predicate,
        ScalarExpr::ConstantExpr(ConstantExpr {
            value,
            ..
        }) if value == &Scalar::Boolean(false) || value == &Scalar::Null
    )
}

fn remove_true_predicate(predicates: Vec<ScalarExpr>) -> Vec<ScalarExpr> {
    predicates.into_iter().filter(|p| !is_true(p)).collect()
}

fn normalize_falsy_predicate(predicates: Vec<ScalarExpr>) -> Vec<ScalarExpr> {
    if predicates.iter().any(is_falsy) {
        vec![
            ConstantExpr {
                span: None,
                value: Scalar::Boolean(false),
            }
            .into(),
        ]
    } else {
        predicates
    }
}

/// Rule to normalize a Filter, including:
/// - Remove true predicates
/// - If there is a NULL or FALSE conjunction, replace the
/// whole filter with FALSE
pub struct RuleNormalizeScalarFilter {
    id: RuleID,
    pattern: SExpr,
}

impl RuleNormalizeScalarFilter {
    pub fn new() -> Self {
        Self {
            id: RuleID::NormalizeScalarFilter,
            // Filter
            //  \
            //   *
            pattern: SExpr::create_unary(
                PatternPlan {
                    plan_type: RelOp::Filter,
                }
                .into(),
                SExpr::create_leaf(
                    PatternPlan {
                        plan_type: RelOp::Pattern,
                    }
                    .into(),
                ),
            ),
        }
    }
}

impl Rule for RuleNormalizeScalarFilter {
    fn id(&self) -> RuleID {
        self.id
    }

    fn apply(
        &self,
        s_expr: &SExpr,
        state: &mut crate::optimizer::rule::TransformResult,
    ) -> Result<()> {
        let mut filter: Filter = s_expr.plan().clone().try_into()?;

        if filter
            .predicates
            .iter()
            .any(|p| is_true(p) || (is_falsy(p) && filter.predicates.len() > 1))
        {
            filter.predicates = normalize_predicates(filter.predicates);
            state.add_result(SExpr::create_unary(filter.into(), s_expr.child(0)?.clone()));
            Ok(())
        } else {
            Ok(())
        }
    }

    fn pattern(&self) -> &SExpr {
        &self.pattern
    }
}
