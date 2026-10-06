#![cfg(test)]

//! Statements that need the PostgreSQL 18 grammar: libpg_query 17 (pg_query
//! 6.x) refuses every one of them with a syntax error. Each must parse into the
//! node shape PostgreSQL 18 gives it and deparse back to the text it came from.
//!
//! The last three tests cover `scan`, `parse` and `fingerprint` on ordinary SQL,
//! which has to keep working across the grammar change.

use pg_query::protobuf::{self, AlterTableType, ConstrType, KeywordKind, ReturningOptionKind, Token};
use pg_query::{fingerprint, parse, scan, FingerprintOptions, NodeEnum};

/// Parses `sql` as a single statement, requires the deparser to reproduce it
/// exactly, and returns the statement node.
fn round_trip(sql: &str) -> NodeEnum {
    let parsed = parse(sql, 0).unwrap_or_else(|error| panic!("{sql:?} does not parse: {error}"));
    let deparsed = parsed.deparse(Default::default()).unwrap_or_else(|error| panic!("{sql:?} does not deparse: {error}"));
    assert_eq!(deparsed, sql);
    assert_eq!(parsed.protobuf.stmts.len(), 1);
    parsed.protobuf.stmts[0].stmt.as_ref().and_then(|stmt| stmt.node.clone()).expect("a statement node")
}

fn names(nodes: &[protobuf::Node]) -> Vec<&str> {
    nodes
        .iter()
        .map(|node| match node.node.as_ref() {
            Some(NodeEnum::String(name)) => name.sval.as_str(),
            other => panic!("expected a name, got {other:?}"),
        })
        .collect()
}

fn create_stmt(sql: &str) -> protobuf::CreateStmt {
    match round_trip(sql) {
        NodeEnum::CreateStmt(stmt) => stmt,
        other => panic!("expected CreateStmt, got {other:?}"),
    }
}

fn last_table_constraint(stmt: &protobuf::CreateStmt) -> &protobuf::Constraint {
    match stmt.table_elts.last().and_then(|elt| elt.node.as_ref()) {
        Some(NodeEnum::Constraint(constraint)) => constraint,
        other => panic!("expected a table constraint, got {other:?}"),
    }
}

/// The constraint added by a single-command `ALTER TABLE ... ADD CONSTRAINT`.
fn added_constraint(sql: &str) -> protobuf::Constraint {
    let NodeEnum::AlterTableStmt(stmt) = round_trip(sql) else { panic!("expected AlterTableStmt") };
    assert_eq!(stmt.cmds.len(), 1);
    let Some(NodeEnum::AlterTableCmd(cmd)) = stmt.cmds[0].node.as_ref() else { panic!("expected AlterTableCmd") };
    assert_eq!(cmd.subtype, AlterTableType::AtAddConstraint as i32);
    match cmd.def.as_ref().and_then(|def| def.node.as_ref()) {
        Some(NodeEnum::Constraint(constraint)) => (**constraint).clone(),
        other => panic!("expected a constraint, got {other:?}"),
    }
}

#[test]
fn it_parses_a_virtual_generated_column() {
    let stmt = create_stmt("CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) VIRTUAL)");
    let Some(NodeEnum::ColumnDef(column)) = stmt.table_elts[1].node.as_ref() else { panic!("expected ColumnDef") };
    assert_eq!(column.colname, "b");
    let Some(NodeEnum::Constraint(generated)) = column.constraints[0].node.as_ref() else { panic!("expected Constraint") };
    assert_eq!(generated.contype, ConstrType::ConstrGenerated as i32);
    assert_eq!(generated.generated_when, "a"); // ALWAYS
    assert_eq!(generated.generated_kind, "v"); // VIRTUAL

    // STORED, the only kind PostgreSQL 17 has, stays distinguishable.
    let stmt = create_stmt("CREATE TABLE t (a int, b int GENERATED ALWAYS AS (a * 2) STORED)");
    let Some(NodeEnum::ColumnDef(column)) = stmt.table_elts[1].node.as_ref() else { panic!("expected ColumnDef") };
    let Some(NodeEnum::Constraint(generated)) = column.constraints[0].node.as_ref() else { panic!("expected Constraint") };
    assert_eq!(generated.generated_kind, "s");
}

#[test]
fn it_parses_a_temporal_primary_key() {
    let stmt = create_stmt("CREATE TABLE p (id int, valid_at tstzrange, PRIMARY KEY (id, valid_at WITHOUT OVERLAPS))");
    let key = last_table_constraint(&stmt);
    assert_eq!(key.contype, ConstrType::ConstrPrimary as i32);
    assert_eq!(names(&key.keys), ["id", "valid_at"]);
    assert!(key.without_overlaps);
}

#[test]
fn it_parses_a_temporal_unique_constraint() {
    let stmt = create_stmt("CREATE TABLE u (id int, valid_at tstzrange, UNIQUE (id, valid_at WITHOUT OVERLAPS))");
    let key = last_table_constraint(&stmt);
    assert_eq!(key.contype, ConstrType::ConstrUnique as i32);
    assert_eq!(names(&key.keys), ["id", "valid_at"]);
    assert!(key.without_overlaps);

    let stmt = create_stmt("CREATE TABLE u (id int, valid_at tstzrange, CONSTRAINT u_key UNIQUE (id, valid_at WITHOUT OVERLAPS))");
    let key = last_table_constraint(&stmt);
    assert_eq!(key.conname, "u_key");
    assert!(key.without_overlaps);
}

#[test]
fn it_parses_a_temporal_foreign_key() {
    let stmt = create_stmt("CREATE TABLE c (id int, valid_at tstzrange, FOREIGN KEY (id, PERIOD valid_at) REFERENCES p (id, PERIOD valid_at))");
    let key = last_table_constraint(&stmt);
    assert_eq!(key.contype, ConstrType::ConstrForeign as i32);
    assert_eq!(names(&key.fk_attrs), ["id", "valid_at"]);
    assert_eq!(names(&key.pk_attrs), ["id", "valid_at"]);
    assert!(key.fk_with_period);
    assert!(key.pk_with_period);
    assert_eq!(key.pktable.as_ref().map(|table| table.relname.as_str()), Some("p"));
}

#[test]
fn it_parses_a_named_not_null_constraint_added_not_valid() {
    let constraint = added_constraint("ALTER TABLE t ADD CONSTRAINT nn NOT NULL a NOT VALID");
    assert_eq!(constraint.contype, ConstrType::ConstrNotnull as i32);
    assert_eq!(constraint.conname, "nn");
    assert_eq!(names(&constraint.keys), ["a"]);
    assert!(constraint.skip_validation);
    assert!(!constraint.initially_valid);
    assert!(!constraint.is_no_inherit);
}

#[test]
fn it_parses_a_named_not_null_constraint_added_no_inherit() {
    let constraint = added_constraint("ALTER TABLE t ADD CONSTRAINT nn NOT NULL a NO INHERIT");
    assert_eq!(constraint.contype, ConstrType::ConstrNotnull as i32);
    assert_eq!(constraint.conname, "nn");
    assert_eq!(names(&constraint.keys), ["a"]);
    assert!(constraint.is_no_inherit);
    assert!(!constraint.skip_validation);
}

#[test]
fn it_parses_returning_with_old_and_new_aliases() {
    let NodeEnum::UpdateStmt(stmt) = round_trip("UPDATE t SET a = 1 RETURNING WITH (OLD AS o, NEW AS n) o.a, n.a") else {
        panic!("expected UpdateStmt")
    };
    let returning = stmt.returning_clause.expect("a RETURNING clause");

    let options: Vec<(i32, &str)> = returning
        .options
        .iter()
        .map(|option| match option.node.as_ref() {
            Some(NodeEnum::ReturningOption(option)) => (option.option, option.value.as_str()),
            other => panic!("expected ReturningOption, got {other:?}"),
        })
        .collect();
    assert_eq!(options, [(ReturningOptionKind::ReturningOptionOld as i32, "o"), (ReturningOptionKind::ReturningOptionNew as i32, "n")]);

    let columns: Vec<Vec<&str>> = returning
        .exprs
        .iter()
        .map(|expr| match expr.node.as_ref() {
            Some(NodeEnum::ResTarget(target)) => match target.val.as_ref().and_then(|val| val.node.as_ref()) {
                Some(NodeEnum::ColumnRef(column)) => names(&column.fields),
                other => panic!("expected ColumnRef, got {other:?}"),
            },
            other => panic!("expected ResTarget, got {other:?}"),
        })
        .collect();
    assert_eq!(columns, [["o", "a"], ["n", "a"]]);
}

#[test]
fn it_still_parses_ordinary_sql() {
    let result = parse("SELECT w.id, w.name FROM widget w JOIN gadget g ON g.widget_id = w.id WHERE w.id = $1 ORDER BY w.name", 0).unwrap();
    let mut tables = result.tables();
    tables.sort();
    assert_eq!(tables, ["gadget", "widget"]);
    assert_eq!(result.statement_types(), ["SelectStmt"]);
    assert_eq!(result.protobuf.version, 180006);

    let result = parse("INSERT INTO widget (id, name) VALUES ($1, $2) RETURNING id; DELETE FROM widget WHERE id = $1", 0).unwrap();
    assert_eq!(result.statement_types(), ["InsertStmt", "DeleteStmt"]);
    let NodeEnum::InsertStmt(insert) = result.protobuf.stmts[0].stmt.as_ref().and_then(|stmt| stmt.node.clone()).unwrap() else {
        panic!("expected InsertStmt")
    };
    // A plain RETURNING list is a ReturningClause with no options.
    let returning = insert.returning_clause.expect("a RETURNING clause");
    assert!(returning.options.is_empty());
    assert_eq!(returning.exprs.len(), 1);
}

#[test]
fn it_still_scans_ordinary_sql() {
    let result = scan("SELECT a FROM t WHERE b = $1").unwrap();
    let tokens: Vec<(Token, KeywordKind, i32, i32)> = result
        .tokens
        .iter()
        .map(|token| (Token::try_from(token.token).unwrap(), KeywordKind::try_from(token.keyword_kind).unwrap(), token.start, token.end))
        .collect();
    assert_eq!(
        tokens,
        [
            (Token::Select, KeywordKind::ReservedKeyword, 0, 6),
            (Token::Ident, KeywordKind::NoKeyword, 7, 8),
            (Token::From, KeywordKind::ReservedKeyword, 9, 13),
            (Token::Ident, KeywordKind::NoKeyword, 14, 15),
            (Token::Where, KeywordKind::ReservedKeyword, 16, 21),
            (Token::Ident, KeywordKind::NoKeyword, 22, 23),
            (Token::Ascii61, KeywordKind::NoKeyword, 24, 25),
            (Token::Param, KeywordKind::NoKeyword, 26, 28),
        ]
    );
}

#[test]
fn it_still_fingerprints_ordinary_sql() {
    // Constants, parameters and layout do not change a fingerprint. With no
    // schema and no alias in play this is also the value libpg_query 17 gives.
    let literal = fingerprint("SELECT id FROM widget WHERE id = 1", 0, 0).unwrap();
    let other = fingerprint("SELECT id FROM widget WHERE id = 4242", 0, 0).unwrap();
    let spaced = fingerprint("SELECT id  FROM widget\n WHERE id = $1", 0, 0).unwrap();
    assert_eq!(literal.hex, "394a2f90c244bffe");
    assert_eq!(literal.value, 0x394a2f90c244bffe);
    assert_eq!(other.hex, literal.hex);
    assert_eq!(spaced.hex, literal.hex);
    assert_ne!(fingerprint("SELECT id FROM gadget WHERE id = 1", 0, 0).unwrap().hex, literal.hex);

    // libpg_query 18.1.0 fingerprints relation references the way Postgres 18
    // computes query IDs: an alias replaces the relation name. The PG17
    // compatibility flags restore libpg_query 17's value
    // (https://github.com/pganalyze/libpg_query/pull/361).
    let aliased = "SELECT w.id FROM widget w WHERE w.id = $1";
    assert_eq!(fingerprint(aliased, 0, 0).unwrap().hex, "74082e2ea437deda");
    assert_eq!(fingerprint(aliased, 0, FingerprintOptions::RANGEVAR_PG17_COMPAT).unwrap().hex, "0743330e3416d46e");

    // Postgres 18 wraps a RETURNING list in a ReturningClause node, so a
    // statement with RETURNING fingerprints differently from libpg_query 17
    // (which gives 07c73da9d1c8fc83 here) even with the PG17 compatibility flags.
    let returning = "INSERT INTO widget (id, name) VALUES ($1, $2) RETURNING id";
    assert_eq!(fingerprint(returning, 0, 0).unwrap().hex, "ed45d6ba674a8398");
    assert_eq!(fingerprint(returning, 0, FingerprintOptions::RANGEVAR_PG17_COMPAT).unwrap().hex, "ed45d6ba674a8398");
}
