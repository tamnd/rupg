//! The elements of `CREATE SCHEMA`, as `transformCreateSchemaStmtElements` checks and sorts them. The caller runs them after the statement makes the schema, as `CreateSchemaCommand` runs them as subcommands.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    AlterTableCmd, AlterTableStmt, AlterTableType, ConstrType, Constraint, CreateStmt, Node,
    ObjectType, RangeVar,
};
use rupg_sql::{Lexer, character, token};

use super::table::constraint_attrs;
use crate::coerce::AtOpt;
use crate::typename::place;

/// The elements of `CREATE SCHEMA`, which the caller runs after the statement made the schema.
#[derive(Debug)]
pub struct SchemaElements {
    /// The owner of the schema. The elements run as this role.
    pub owner: u32,
    /// The name of the schema, which comes first in `search_path` while the elements run.
    pub schema: String,
    /// The statements in the order that they run, each with its part of the text of the statement.
    pub elements: Vec<(Node, String)>,
}

/// `transformCreateSchemaStmtElements`: checks that the elements make their objects in the schema `schema` and sorts them so that an element comes after the objects that it refers to: sequences, tables, views, indexes, triggers, grants and then the foreign keys of the tables. `text` is the text of the statement.
pub(super) fn schema_elements(
    elements: &[Node],
    schema: &str,
    text: &str,
) -> Result<Vec<(Node, String)>> {
    let texts = element_texts(text, elements.len());
    let mut groups: [Vec<(Node, String)>; 7] = Default::default();
    for (element, text) in elements.iter().zip(texts) {
        let (group, relation) = match element {
            Node::CreateSeqStmt(stmt) => (0, stmt.sequence.as_deref()),
            Node::CreateStmt(stmt) => (1, stmt.relation.as_deref()),
            Node::ViewStmt(stmt) => (2, stmt.view.as_deref()),
            Node::IndexStmt(stmt) => (3, stmt.relation.as_deref()),
            Node::CreateTrigStmt(stmt) => (4, stmt.relation.as_deref()),
            Node::GrantStmt(_) => (5, None),
            _ => return Err(Error::internal("unrecognized node type in CREATE SCHEMA")),
        };
        if let Some(relation) = relation {
            check_schema_name(relation, schema)?;
        }
        let element = match element {
            Node::CreateStmt(stmt) => {
                let mut keys = Vec::new();
                let table = without_foreign_keys(stmt, &mut keys)?;
                groups[6].extend(keys.into_iter().map(|key| (key, String::new())));
                Node::CreateStmt(Box::new(table))
            }
            element => element.clone(),
        };
        groups[group].push((element, text));
    }
    Ok(groups.into_iter().flatten().collect())
}

/// `checkSchemaNameRV`: an element can name only the new schema, and it cannot make a temporary relation.
fn check_schema_name(relation: &RangeVar, schema: &str) -> Result<()> {
    let at = place(relation.location);
    if let Some(name) = relation.schemaname.as_deref()
        && name != schema
    {
        return Err(Error::new(
            SqlState::INVALID_SCHEMA_DEFINITION,
            format!(
                "CREATE specifies a schema ({name}) different from the one being created ({schema})"
            ),
        )
        .at_opt(at));
    }
    if relation.relpersistence == b't' {
        return Err(Error::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "cannot create temporary relation in non-temporary schema",
        )
        .at_opt(at));
    }
    Ok(())
}

/// `transformCreateSchemaCreateTable`: the table without its foreign keys. Each foreign key becomes an `ALTER TABLE ... ADD CONSTRAINT` in `keys`, so that it can refer to a table of a later element.
fn without_foreign_keys(stmt: &CreateStmt, keys: &mut Vec<Node>) -> Result<CreateStmt> {
    let add = |con: Constraint| {
        Node::AlterTableStmt(Box::new(AlterTableStmt {
            relation: stmt.relation.clone(),
            cmds: vec![Some(Node::AlterTableCmd(Box::new(AlterTableCmd {
                subtype: AlterTableType::AT_AddConstraint,
                def: Some(Node::Constraint(Box::new(con))),
                ..AlterTableCmd::default()
            })))],
            objtype: ObjectType::OBJECT_TABLE,
            missing_ok: false,
        }))
    };
    let mut table = stmt.clone();
    table.tableElts.clear();
    for element in &stmt.tableElts {
        match element {
            Some(Node::Constraint(con)) if con.contype == ConstrType::CONSTR_FOREIGN => {
                keys.push(add((**con).clone()));
            }
            Some(Node::ColumnDef(def)) => {
                let mut cons: Vec<Constraint> = def
                    .constraints
                    .iter()
                    .flatten()
                    .filter_map(|n| match n {
                        Node::Constraint(c) => Some((**c).clone()),
                        _ => None,
                    })
                    .collect();
                constraint_attrs(&mut cons)?;
                // The attributes after a foreign key go with it, as `transformConstraintAttrs` put them into the key.
                let mut after_key = false;
                let mut kept = Vec::new();
                for mut con in cons {
                    match con.contype {
                        ConstrType::CONSTR_FOREIGN => {
                            let name = def.colname.as_deref().unwrap_or_default();
                            con.fk_attrs = vec![Some(Node::String(name.into()))];
                            keys.push(add(con));
                            after_key = true;
                        }
                        ConstrType::CONSTR_ATTR_DEFERRABLE
                        | ConstrType::CONSTR_ATTR_NOT_DEFERRABLE
                        | ConstrType::CONSTR_ATTR_DEFERRED
                        | ConstrType::CONSTR_ATTR_IMMEDIATE
                        | ConstrType::CONSTR_ATTR_ENFORCED
                        | ConstrType::CONSTR_ATTR_NOT_ENFORCED => {
                            if !after_key {
                                kept.push(con);
                            }
                        }
                        _ => {
                            after_key = false;
                            kept.push(con);
                        }
                    }
                }
                let mut def = (**def).clone();
                def.constraints =
                    kept.into_iter().map(|con| Some(Node::Constraint(Box::new(con)))).collect();
                table.tableElts.push(Some(Node::ColumnDef(Box::new(def))));
            }
            element => table.tableElts.push(element.clone()),
        }
    }
    Ok(table)
}

/// The part of `text` that each of the `count` elements of `CREATE SCHEMA` has. An element starts at `CREATE` or `GRANT` outside parentheses, but not at the first word, at a privilege `CREATE` after `GRANT` or a comma, or at the `GRANT` of `WITH GRANT OPTION`. When the words do not give `count` elements, each element has the whole text.
fn element_texts(text: &str, count: usize) -> Vec<String> {
    let (open, close, comma) = (character(b'('), character(b')'), character(b','));
    let mut lexer = Lexer::new(text);
    let mut starts = Vec::new();
    let mut depth = 0usize;
    let mut previous = None;
    while let Ok(word) = lexer.next_token() {
        let kind = Some(word.kind);
        if word.kind == 0 {
            break;
        } else if kind == open {
            depth += 1;
        } else if kind == close {
            depth = depth.saturating_sub(1);
        } else if depth == 0 && previous.is_some() {
            let create =
                word.kind == token::CREATE && previous != Some(token::GRANT) && previous != comma;
            let grant = word.kind == token::GRANT && previous != Some(token::WITH);
            if create || grant {
                starts.push(word.start);
            }
        }
        previous = kind;
    }
    if starts.len() != count {
        return vec![text.to_string(); count];
    }
    let ends = starts.iter().skip(1).copied().chain([text.len()]);
    starts.iter().zip(ends).map(|(&start, end)| text[start..end].trim_end().to_string()).collect()
}
