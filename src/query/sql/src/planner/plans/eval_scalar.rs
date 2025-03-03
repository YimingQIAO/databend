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
// limitations under the License.#[derive(Clone, Debug)]

use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::Result;

use crate::optimizer::ColumnSet;
use crate::optimizer::ColumnStatSet;
use crate::optimizer::PhysicalProperty;
use crate::optimizer::RelExpr;
use crate::optimizer::RelationalProperty;
use crate::optimizer::RequiredProperty;
use crate::optimizer::Statistics;
use crate::plans::Operator;
use crate::plans::RelOp;
use crate::plans::ScalarExpr;
use crate::IndexType;

/// Evaluate scalar expression
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EvalScalar {
    pub items: Vec<ScalarItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScalarItem {
    pub scalar: ScalarExpr,
    pub index: IndexType,
}

impl EvalScalar {
    pub fn used_columns(&self) -> Result<ColumnSet> {
        let mut used_columns = ColumnSet::new();
        for item in self.items.iter() {
            used_columns.insert(item.index);
            used_columns.extend(item.scalar.used_columns());
        }
        Ok(used_columns)
    }
}

impl Operator for EvalScalar {
    fn rel_op(&self) -> RelOp {
        RelOp::EvalScalar
    }

    fn derive_physical_prop(&self, rel_expr: &RelExpr) -> Result<PhysicalProperty> {
        rel_expr.derive_physical_prop_child(0)
    }

    fn compute_required_prop_child(
        &self,
        _ctx: Arc<dyn TableContext>,
        _rel_expr: &RelExpr,
        _child_index: usize,
        required: &RequiredProperty,
    ) -> Result<RequiredProperty> {
        Ok(required.clone())
    }

    fn derive_relational_prop(&self, rel_expr: &RelExpr) -> Result<RelationalProperty> {
        let input_prop = rel_expr.derive_relational_prop_child(0)?;

        // Derive output columns
        let mut output_columns = input_prop.output_columns;
        for item in self.items.iter() {
            output_columns.insert(item.index);
        }

        // Derive outer columns
        let mut outer_columns = input_prop.outer_columns;
        for item in self.items.iter() {
            let used_columns = item.scalar.used_columns();
            let outer = used_columns
                .difference(&output_columns)
                .cloned()
                .collect::<ColumnSet>();
            outer_columns = outer_columns.union(&outer).cloned().collect();
        }
        outer_columns = outer_columns.difference(&output_columns).cloned().collect();

        // Derive cardinality
        let cardinality = input_prop.cardinality;
        let precise_cardinality = input_prop.statistics.precise_cardinality;
        let is_accurate = input_prop.statistics.is_accurate;
        // Derive used columns
        let mut used_columns = self.used_columns()?;
        used_columns.extend(input_prop.used_columns);

        let mut column_stats: ColumnStatSet = Default::default();
        for (k, v) in input_prop.statistics.column_stats {
            if !used_columns.contains(&k) {
                continue;
            }
            column_stats.insert(k as IndexType, v);
        }

        Ok(RelationalProperty {
            output_columns,
            outer_columns,
            used_columns,
            cardinality,
            statistics: Statistics {
                precise_cardinality,
                column_stats,
                is_accurate,
            },
        })
    }
}
