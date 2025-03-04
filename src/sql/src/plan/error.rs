// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::error::Error;
use std::fmt;
use std::num::ParseIntError;
use std::num::TryFromIntError;

use expr::EvalError;
use ore::stack::RecursionLimitError;
use ore::str::StrExt;
use repr::strconv::ParseError;
use repr::ColumnName;

use crate::catalog::CatalogError;
use crate::names::PartialName;
use crate::plan::plan_utils::JoinSide;
use crate::plan::scope::ScopeItem;

#[derive(Clone, Debug)]
pub enum PlanError {
    Unsupported {
        feature: String,
        issue_no: Option<usize>,
    },
    UnknownColumn {
        table: Option<PartialName>,
        column: ColumnName,
    },
    UngroupedColumn {
        table: Option<PartialName>,
        column: ColumnName,
    },
    WrongJoinTypeForLateralColumn {
        table: Option<PartialName>,
        column: ColumnName,
    },
    AmbiguousColumn(ColumnName),
    AmbiguousTable(PartialName),
    UnknownColumnInUsingClause {
        column: ColumnName,
        join_side: JoinSide,
    },
    AmbiguousColumnInUsingClause {
        column: ColumnName,
        join_side: JoinSide,
    },
    MisqualifiedName(String),
    OverqualifiedDatabaseName(String),
    OverqualifiedSchemaName(String),
    SubqueriesDisallowed {
        context: String,
    },
    UnknownParameter(usize),
    RecursionLimit(RecursionLimitError),
    Parse(ParseError),
    Catalog(CatalogError),
    UpsertSinkWithoutKey,
    // TODO(benesch): eventually all errors should be structured.
    Unstructured(String),
}

impl PlanError {
    pub(crate) fn ungrouped_column(item: &ScopeItem) -> PlanError {
        PlanError::UngroupedColumn {
            table: item.table_name.clone(),
            column: item.column_name.clone(),
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Unsupported { feature, issue_no } => {
                write!(f, "{} not yet supported", feature)?;
                if let Some(issue_no) = issue_no {
                    write!(f, ", see https://github.com/MaterializeInc/materialize/issues/{} for more details", issue_no)?;
                }
                Ok(())
            }
            Self::UnknownColumn { table, column } => write!(
                f,
                "column {} does not exist",
                ColumnDisplay { table, column }
            ),
            Self::UngroupedColumn { table, column } => write!(
                f,
                "column {} must appear in the GROUP BY clause or be used in an aggregate function",
                ColumnDisplay { table, column },
            ),
            Self::WrongJoinTypeForLateralColumn { table, column } => write!(
                f,
                "column {} cannot be referenced from this part of the query: \
                the combining JOIN type must be INNER or LEFT for a LATERAL reference",
                ColumnDisplay { table, column },
            ),
            Self::AmbiguousColumn(column) => write!(
                f,
                "column reference {} is ambiguous",
                column.as_str().quoted()
            ),
            Self::AmbiguousTable(table) => write!(
                f,
                "table reference {} is ambiguous",
                table.item.as_str().quoted()
            ),
            Self::UnknownColumnInUsingClause { column, join_side } => write!(
                f,
                "column {} specified in USING clause does not exist in {} table",
                column.as_str().quoted(),
                join_side,
            ),
            Self::AmbiguousColumnInUsingClause { column, join_side } => write!(
                f,
                "common column name {} appears more than once in {} table",
                column.as_str().quoted(),
                join_side,
            ),
            Self::MisqualifiedName(name) => write!(
                f,
                "qualified name did not have between 1 and 3 components: {}",
                name
            ),
            Self::OverqualifiedDatabaseName(name) => write!(
                f,
                "database name '{}' does not have exactly one component",
                name
            ),
            Self::OverqualifiedSchemaName(name) => write!(
                f,
                "schema name '{}' cannot have more than two components",
                name
            ),
            Self::SubqueriesDisallowed { context } => {
                write!(f, "{} does not allow subqueries", context)
            }
            Self::UnknownParameter(n) => write!(f, "there is no parameter ${}", n),
            Self::RecursionLimit(e) => write!(f, "{}", e),
            Self::Parse(e) => write!(f, "{}", e),
            Self::Catalog(e) => write!(f, "{}", e),
            Self::UpsertSinkWithoutKey => write!(f, "upsert sinks must specify a key"),
            Self::Unstructured(e) => write!(f, "{}", e),
        }
    }
}

impl Error for PlanError {}

impl From<CatalogError> for PlanError {
    fn from(e: CatalogError) -> PlanError {
        PlanError::Catalog(e)
    }
}

impl From<ParseError> for PlanError {
    fn from(e: ParseError) -> PlanError {
        PlanError::Parse(e)
    }
}

impl From<RecursionLimitError> for PlanError {
    fn from(e: RecursionLimitError) -> PlanError {
        PlanError::RecursionLimit(e)
    }
}

impl From<anyhow::Error> for PlanError {
    fn from(e: anyhow::Error) -> PlanError {
        PlanError::Unstructured(format!("{:#}", e))
    }
}

impl From<TryFromIntError> for PlanError {
    fn from(e: TryFromIntError) -> PlanError {
        PlanError::Unstructured(format!("{:#}", e))
    }
}

impl From<ParseIntError> for PlanError {
    fn from(e: ParseIntError) -> PlanError {
        PlanError::Unstructured(format!("{:#}", e))
    }
}

impl From<EvalError> for PlanError {
    fn from(e: EvalError) -> PlanError {
        PlanError::Unstructured(format!("{:#}", e))
    }
}

struct ColumnDisplay<'a> {
    table: &'a Option<PartialName>,
    column: &'a ColumnName,
}

impl<'a> fmt::Display for ColumnDisplay<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(table) = &self.table {
            format!("{}.{}", table.item, self.column).quoted().fmt(f)
        } else {
            self.column.as_str().quoted().fmt(f)
        }
    }
}
