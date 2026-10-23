//! One bounded-memory aggregation engine for the runtime ledger and legacy imports.

use crate::report::{GroupAgg, Measure, Query};
use crate::{Config, Envelope, KindSpec, SinkKind};
use rusqlite::{Connection, functions::FunctionFlags, types::Value};
use std::io::BufRead;

fn io(e: impl std::fmt::Display) -> crate::Error {
    crate::Error::Io(e.to_string())
}

pub(crate) fn functions(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("hatel_epoch", 1, flags, |c| {
        Ok(c.get::<Option<String>>(0)?
            .and_then(|value| value.parse::<jiff::Timestamp>().ok())
            .map(|t| t.as_second()))
    })?;
    conn.create_scalar_function("hatel_nanos", 1, flags, |c| {
        Ok(c.get::<Option<String>>(0)?
            .and_then(|value| value.parse::<jiff::Timestamp>().ok())
            .map(|t| t.subsec_nanosecond()))
    })
}

fn path(field: &str) -> String {
    format!(
        "$.{}",
        serde_json::to_string(field).expect("field serializes")
    )
}

fn label_in(document: &str, expression: &str) -> String {
    format!(
        "CASE json_type({document}, {expression}) WHEN 'text' THEN json_extract({document}, {expression}) WHEN 'true' THEN 'true' WHEN 'false' THEN 'false' WHEN 'null' THEN 'null' ELSE json_quote(json_extract({document}, {expression})) END"
    )
}

fn label(expression: &str) -> String {
    label_in("payload", expression)
}

fn selector(field: &str) -> (&'static str, String) {
    if crate::registry::METADATA_FIELDS.contains(&field) {
        (
            "metadata",
            format!(
                "$.{}",
                field
                    .strip_prefix("metadata.")
                    .expect("known metadata field")
            ),
        )
    } else {
        ("payload", path(field))
    }
}

fn connection(cfg: &Config, kind: &str) -> crate::Result<Connection> {
    let conn = if cfg.sink == SinkKind::Sqlite {
        let db = cfg.state_dir.join("telemetry.db");
        if !db.exists() {
            scratch()?
        } else {
            Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(io)?
        }
    } else {
        let mut conn = scratch()?;
        let tx = conn.transaction().map_err(io)?;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO records (ts, kind, schema_version, payload, metadata) VALUES (?1,?2,?3,?4,?5)",
                )
                .map_err(io)?;
            for (_, file) in
                crate::rolling::open_snapshot(&cfg.ledger_dir, &format!("{kind}.jsonl"))
                    .map_err(io)?
            {
                for line in std::io::BufReader::new(file).lines() {
                    let line = line.map_err(io)?;
                    let Some(env) = Envelope::from_json_line(&line) else {
                        continue;
                    };
                    if env.kind != kind {
                        continue;
                    }
                    insert
                        .execute(rusqlite::params![
                            env.ts,
                            env.kind,
                            env.schema_version,
                            serde_json::to_string(&env.payload).map_err(io)?,
                            serde_json::to_string(&env.metadata).map_err(io)?
                        ])
                        .map_err(io)?;
                }
            }
        }
        tx.commit().map_err(io)?;
        conn
    };
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(io)?;
    conn.execute_batch("PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;")
        .map_err(io)?;
    functions(&conn).map_err(io)?;
    let columns = conn
        .prepare("PRAGMA table_info(records)")
        .map_err(io)?
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(io)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(io)?;
    let metadata = if columns.iter().any(|c| c == "metadata") {
        "metadata"
    } else {
        "'{}'"
    };
    let seconds = if columns.iter().any(|c| c == "occurred_seconds") {
        "occurred_seconds"
    } else {
        "NULL"
    };
    conn.execute_batch(&format!("CREATE TEMP VIEW records_read AS SELECT rowid AS sequence, ts,kind,schema_version,payload,{metadata} AS metadata,{seconds} AS occurred_seconds FROM records;")).map_err(io)?;
    Ok(conn)
}

fn scratch() -> crate::Result<Connection> {
    let conn = Connection::open("").map_err(io)?;
    conn.execute_batch("PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
        CREATE TABLE records (ts TEXT NOT NULL, kind TEXT NOT NULL, schema_version INTEGER NOT NULL, payload TEXT NOT NULL, metadata TEXT NOT NULL DEFAULT '{}');").map_err(io)?;
    functions(&conn).map_err(io)?;
    Ok(conn)
}

pub struct SectionResult {
    pub groups: crate::Result<Vec<GroupAgg>>,
    pub retained_since: Option<String>,
    pub contracts: Vec<ContractCount>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContractCount {
    pub schema_id: Option<String>,
    pub kind_version: Option<u32>,
    pub count: i64,
}

pub fn aggregate(cfg: &Config, spec: &KindSpec, q: &Query) -> crate::Result<Vec<GroupAgg>> {
    section(cfg, spec, q).and_then(|result| result.groups)
}

pub fn section(cfg: &Config, spec: &KindSpec, q: &Query) -> crate::Result<SectionResult> {
    use rusqlite::OptionalExtension;
    let mut conn = connection(cfg, &spec.name)?;
    let transaction = conn.transaction().map_err(io)?;
    let oldest = Query {
        since: i64::MIN,
        ..*q
    };
    let mut oldest_args = vec![
        spec.name.clone().into(),
        oldest.since.into(),
        i64::from(crate::SCHEMA_VERSION).into(),
    ];
    let mut oldest_scope = scope(spec, &oldest, &mut oldest_args);
    oldest_scope.push("hatel_epoch(ts) IS NOT NULL".into());
    let retained_since=transaction.query_row(&format!("SELECT ts FROM records_read WHERE {} ORDER BY hatel_epoch(ts),hatel_nanos(ts),sequence LIMIT 1",oldest_scope.join(" AND ")),rusqlite::params_from_iter(oldest_args),|r|r.get(0)).optional().map_err(io)?;
    let contracts = contract_counts(&transaction, spec, q)?;
    let groups = validate_contracts(spec, q, &contracts)
        .and_then(|()| aggregate_connection(&transaction, spec, q));
    Ok(SectionResult {
        groups,
        retained_since,
        contracts,
    })
}

fn scope(spec: &KindSpec, q: &Query, args: &mut Vec<Value>) -> Vec<String> {
    let mut conditions = vec![
        "kind=?1".into(),
        "(occurred_seconds>=?2 OR (occurred_seconds IS NULL AND hatel_epoch(ts)>=?2))".into(),
        "schema_version<=?3".into(),
    ];
    if let Some(project) = q.project {
        let p = parameter(args, project.into());
        let legacy = parameter(args, payload_project(spec, project));
        conditions.push(format!("(json_extract(metadata, '$.project.key')={p} OR json_extract(metadata, '$.project.label')={p} OR (json_extract(metadata, '$.project') IS NULL AND json_extract(payload, '$.project')={legacy}))"));
    }
    if let Some(contract) = q.contract {
        if contract == "legacy" {
            if let Some((_, id)) = q
                .filters
                .iter()
                .find(|(field, _)| field == "metadata.schema_id")
            {
                let p = parameter(args, id.clone());
                conditions.push(format!("json_extract(metadata,'$.schema_id')={p}"));
            } else {
                conditions.push("json_extract(metadata,'$.schema_id') IS NULL".into());
            }
        } else {
            let p = parameter(args, contract.into());
            conditions.push(format!("json_extract(metadata,'$.schema_id')={p}"));
        }
    }
    conditions
}

fn filter_conditions(spec: &KindSpec, q: &Query, args: &mut Vec<Value>) -> Vec<String> {
    let mut filters = Vec::new();
    for (field, value) in q.filters {
        if field == "project" {
            let legacy = parameter(args, payload_project(spec, value));
            let value = parameter(args, value.clone());
            filters.push(format!("(json_extract(metadata,'$.project.key')={value} OR (json_extract(metadata,'$.project') IS NULL AND json_extract(payload,'$.project')={legacy}))"));
        } else {
            let (document, path) = selector(field);
            let path = parameter(args, path);
            let value = parameter(args, value.clone());
            filters.push(format!(
                "json_type({document},{path}) IS NOT NULL AND {}={value}",
                label_in(document, &path)
            ));
        }
    }
    filters
}

fn contract_counts(
    conn: &Connection,
    spec: &KindSpec,
    q: &Query,
) -> crate::Result<Vec<ContractCount>> {
    let mut args = vec![
        spec.name.clone().into(),
        q.since.into(),
        i64::from(crate::SCHEMA_VERSION).into(),
    ];
    let mut conditions = scope(spec, q, &mut args);
    if spec.mode == crate::registry::KindMode::Event {
        conditions.extend(filter_conditions(spec, q, &mut args));
    }
    let mut stmt=conn.prepare(&format!("SELECT json_extract(metadata,'$.schema_id'),json_extract(metadata,'$.kind_version'),COUNT(*) FROM records_read WHERE {} GROUP BY 1,2 ORDER BY 1,2",conditions.join(" AND "))).map_err(io)?;
    stmt.query_map(rusqlite::params_from_iter(args), |r| {
        Ok(ContractCount {
            schema_id: r.get(0)?,
            kind_version: r.get(1)?,
            count: r.get(2)?,
        })
    })
    .map_err(io)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(io)
}

fn validate_contracts(
    spec: &KindSpec,
    q: &Query,
    contracts: &[ContractCount],
) -> crate::Result<()> {
    let valid = if q.contract == Some("legacy") {
        contracts.len() <= 1
    } else {
        contracts.iter().all(|c| {
            c.schema_id.as_deref() == Some(spec.schema_id.as_str())
                && c.kind_version.is_none_or(|v| v == spec.version)
        })
    };
    if valid {
        return Ok(());
    }
    Err(crate::Error::InvalidPayload(format!(
        "{} contains mixed or unverified contracts {}; select one with --contract <schema_id>, or explicitly interpret one legacy contract with --contract legacy and its exact historical schema",
        spec.name,
        serde_json::to_string(contracts).expect("contract counts serialize")
    )))
}

fn payload_project(spec: &KindSpec, project: &str) -> String {
    if spec.redact.contains("project") {
        crate::pii::redacted(project)
    } else {
        project.into()
    }
}

fn parameter(args: &mut Vec<Value>, value: String) -> String {
    args.push(value.into());
    format!("?{}", args.len())
}

fn aggregate_connection(
    conn: &Connection,
    spec: &KindSpec,
    q: &Query,
) -> crate::Result<Vec<GroupAgg>> {
    let mut args: Vec<Value> = vec![
        spec.name.clone().into(),
        q.since.into(),
        i64::from(crate::SCHEMA_VERSION).into(),
    ];
    let (document, field_path) = selector(q.group_by.unwrap_or(&spec.group_key));
    let dimension = parameter(&mut args, field_path);
    let dimension_label = if q.group_by.unwrap_or(&spec.group_key) == "project" {
        format!(
            "COALESCE(json_extract(metadata, '$.project.key'), {})",
            label(&dimension)
        )
    } else {
        label_in(document, &dimension)
    };
    let conditions = scope(spec, q, &mut args);
    let key = if q.group_by.unwrap_or(&spec.group_key) == "project" {
        format!(
            "CASE WHEN json_type(payload, {dimension}) IS NULL AND json_extract(metadata, '$.project.key') IS NULL THEN NULL ELSE {dimension_label} END"
        )
    } else {
        format!(
            "CASE WHEN json_type({document}, {dimension}) IS NULL THEN NULL ELSE {dimension_label} END"
        )
    };
    let identity = spec.identity.as_ref().map(|field| {
        let p = parameter(&mut args, path(field));
        if spec.mode == crate::registry::KindMode::Snapshot {
            format!("CASE WHEN json_type(payload, {p})='text' THEN json_extract(payload, {p}) END")
        } else {
            format!(
                "CASE WHEN json_type(payload, {p}) IS NULL THEN NULL ELSE {} END",
                label(&p)
            )
        }
    });
    let mut sql = format!(
        "WITH scoped AS (SELECT sequence, ts, payload, metadata, {key} AS key {} FROM records_read WHERE {})",
        identity
            .as_ref()
            .map(|i| format!(", {i} AS identity"))
            .unwrap_or_default(),
        conditions.join(" AND ")
    );
    let selected = if spec.mode == crate::registry::KindMode::Snapshot {
        let invalid = conn.query_row(
            &format!("{sql} SELECT EXISTS(SELECT 1 FROM scoped WHERE identity IS NULL OR identity='' OR hatel_epoch(json_extract(metadata,'$.observed_at')) IS NULL)"),
            rusqlite::params_from_iter(args.iter()), |row| row.get::<_, bool>(0),
        ).map_err(io)?;
        if invalid {
            return Err(crate::Error::InvalidPayload(format!(
                "snapshot Kind '{}' contains a record without a non-empty string identity or valid observation time",
                spec.name
            )));
        }
        let entity = "COALESCE(json_extract(metadata,'$.project.key'), json_extract(metadata,'$.project.label'), json_extract(payload,'$.project')), identity";
        let observed = "json_extract(metadata,'$.observed_at')";
        sql.push_str(&format!(", versions AS (SELECT *, DENSE_RANK() OVER (PARTITION BY {entity} ORDER BY hatel_epoch(ts) DESC, hatel_nanos(ts) DESC, hatel_epoch({observed}) DESC, hatel_nanos({observed}) DESC) AS revision FROM scoped), latest AS (SELECT * FROM versions WHERE revision=1)"));
        let ambiguous = conn.query_row(
            &format!("{sql} SELECT EXISTS(SELECT 1 FROM latest GROUP BY {entity} HAVING COUNT(DISTINCT payload)>1)"),
            rusqlite::params_from_iter(args.iter()), |row| row.get::<_, bool>(0),
        ).map_err(io)?;
        if ambiguous {
            return Err(crate::Error::InvalidPayload(format!(
                "snapshot Kind '{}' has conflicting states with identical occurrence and observation times",
                spec.name
            )));
        }
        sql.push_str(&format!(", representatives AS (SELECT *, ROW_NUMBER() OVER (PARTITION BY {entity} ORDER BY sequence DESC) AS duplicate FROM latest), selected AS (SELECT * FROM representatives WHERE duplicate=1)"));
        "selected"
    } else {
        "scoped"
    };
    let filters = filter_conditions(spec, q, &mut args);
    sql.push_str(&format!(
        ", matching AS (SELECT * FROM {selected} WHERE {})",
        if filters.is_empty() {
            "1".into()
        } else {
            filters.join(" AND ")
        }
    ));
    let from = if identity.is_some() && spec.mode == crate::registry::KindMode::Event {
        sql.push_str(", ranked AS (SELECT *, ROW_NUMBER() OVER (PARTITION BY key, identity ORDER BY hatel_epoch(ts), hatel_nanos(ts), sequence) AS ordinal FROM matching)");
        "ranked WHERE identity IS NULL OR ordinal=1"
    } else {
        "matching"
    };
    let mut columns = vec!["key".to_string(), "COUNT(*)".into()];
    let mut layouts = Vec::new();
    for measure in &spec.measures {
        let p = parameter(&mut args, path(measure));
        let value = format!("json_extract(payload, {p})");
        let valid = format!(
            "json_type(payload, {p}) IN ('integer','real') AND abs(CAST({value} AS REAL))<=1.7976931348623157e308"
        );
        let start = columns.len();
        columns.extend([
            format!("SUM(CASE WHEN {valid} THEN CAST({value} AS REAL) END)"),
            format!("COUNT(CASE WHEN {valid} THEN 1 END)"),
            format!("COUNT(CASE WHEN json_type(payload, {p}) IS NULL THEN 1 END)"),
            format!("AVG(CASE WHEN {valid} THEN CAST({value} AS REAL) END)"),
            format!("MIN(CASE WHEN {valid} THEN CAST({value} AS REAL) END)"),
            format!("MAX(CASE WHEN {valid} THEN CAST({value} AS REAL) END)"),
        ]);
        let scale = match spec.field_specs[measure].unit.as_deref() {
            Some("s") => Some(1.0),
            Some("ms") => Some(1000.0),
            _ => None,
        };
        let bounds: Vec<f64> = scale
            .map(|scale| {
                [0.01, 0.1, 1.0, 10.0, 60.0, 300.0]
                    .map(|b| b * scale)
                    .to_vec()
            })
            .unwrap_or_default();
        for bound in &bounds {
            columns.push(format!(
                "COUNT(CASE WHEN {valid} AND {value}<={bound} THEN 1 END)"
            ));
        }
        layouts.push((start, bounds));
    }
    sql.push_str(&format!(
        " SELECT {} FROM {from} GROUP BY key",
        columns.join(",")
    ));
    let sort = q.sort_by.or(spec.measures.first().map(String::as_str));
    let sort_column = sort
        .and_then(|s| spec.measures.iter().position(|m| m == s))
        .map_or(2, |i| layouts[i].0 + 1);
    sql.push_str(&format!(" ORDER BY {sort_column} DESC, key IS NULL, key"));
    if q.top_n > 0 {
        sql.push_str(&format!(" LIMIT {}", q.top_n));
    }
    let mut stmt = conn.prepare(&sql).map_err(io)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args), |row| {
            let count: i64 = row.get(1)?;
            let sums = spec
                .measures
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let (start, bounds) = &layouts[i];
                    let start = *start;
                    let valid_count: i64 = row.get(start + 1)?;
                    let missing_count: i64 = row.get(start + 2)?;
                    let sum: Option<f64> = row.get(start)?;
                    let avg: Option<f64> = row.get(start + 3)?;
                    if (valid_count > 0 && (sum.is_none() || avg.is_none()))
                        || [sum, avg].into_iter().flatten().any(|n| !n.is_finite())
                    {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            start,
                            rusqlite::types::Type::Real,
                            Box::new(std::io::Error::other(format!(
                                "{name} aggregate exceeds the finite numeric range"
                            ))),
                        ));
                    }
                    Ok(Measure {
                        name: name.clone(),
                        sum: sum.unwrap_or(0.0),
                        valid_count,
                        missing_count,
                        invalid_count: count - valid_count - missing_count,
                        avg,
                        min: row.get(start + 4)?,
                        max: row.get(start + 5)?,
                        unit: spec.field_specs[name].unit.clone(),
                        buckets: bounds
                            .iter()
                            .enumerate()
                            .map(|(j, upper_bound)| {
                                Ok(crate::report::Bucket {
                                    upper_bound: *upper_bound,
                                    count: row.get(start + 6 + j)?,
                                })
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    })
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(GroupAgg {
                key: row.get(0)?,
                count,
                sums,
            })
        })
        .map_err(io)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(io)
}
