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

use std::fmt::Display;
use std::fmt::Formatter;

use crate::ast::write_comma_separated_list;
use crate::ast::Identifier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnSetStmt {
    pub source: UnSetSource,
}

impl Display for UnSetStmt {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        write!(f, "UNSET ")?;
        write!(f, "{}", self.source)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnSetSource {
    Var { variable: Identifier },
    Vars { variables: Vec<Identifier> },
}

impl Display for UnSetSource {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            UnSetSource::Var { variable } => write!(f, "{variable}"),
            UnSetSource::Vars { variables } => {
                write!(f, "(")?;
                write_comma_separated_list(f, variables)?;
                write!(f, ")")
            }
        }
    }
}
