//! A local stand-in of Cloudflare D1's REST API, as smugglr's own d1 tests use: a SQLite file
//! behind a small server in this test process. D1 is never reached.
//!
//! It keeps the D1 quirks a shipped migration meets: a bearer token; databases found by a name
//! filter that also matches other names; text-only parameters; integers back as plain JSON
//! numbers; its own `_cf_*` tables refused; foreign keys always on, whatever a pragma says; TEMP
//! tables and triggers refused. It runs a batch as one transaction.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Value as Json, json};

const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
const TOKEN: &str = "stand-in-token";
const UUID: &str = "01a15879-0000-7000-8000-00000000d1d1";

/// Serves the D1 database `name`, kept in the SQLite file `path`, until the test process ends;
/// returns the environment smugglr reaches it with.
pub fn serve(name: &str, path: &str) -> Vec<(String, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the D1 stand-in");
    let address = listener.local_addr().expect("the stand-in's address");
    let api = format!("http://{address}/client/v4");
    let (name, path) = (name.to_owned(), path.to_owned());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = answer(stream, &name, &path);
        }
    });
    vec![
        ("CLOUDFLARE_ACCOUNT_ID".to_owned(), ACCOUNT.to_owned()),
        ("CLOUDFLARE_API_TOKEN".to_owned(), TOKEN.to_owned()),
        ("CLOUDFLARE_API_BASE_URL".to_owned(), api),
    ]
}

fn answer(mut stream: TcpStream, name: &str, path: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((field, value)) = header.split_once(':') {
            if field.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().unwrap_or(0);
            } else if field.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let (status, envelope) = match handle(line.trim(), authorization.as_deref(), &body, name, path)
    {
        Ok(result) => (
            "200 OK",
            json!({"success": true, "errors": [], "messages": [], "result": result}),
        ),
        Err((status, code, message)) => (
            status,
            json!({
                "success": false,
                "errors": [{"code": code, "message": message}],
                "messages": [],
                "result": null,
            }),
        ),
    };
    let envelope = envelope.to_string();
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{envelope}",
        envelope.len()
    )
}

type Refusal = (&'static str, u32, String);

fn handle(
    line: &str,
    authorization: Option<&str>,
    body: &[u8],
    name: &str,
    path: &str,
) -> Result<Json, Refusal> {
    if authorization != Some(&format!("Bearer {TOKEN}")) {
        return Err(("401 Unauthorized", 10000, "Authentication error".to_owned()));
    }
    let databases = format!("/client/v4/accounts/{ACCOUNT}/d1/database");
    let (method, target) = line
        .split_once(' ')
        .map(|(method, rest)| (method, rest.split(' ').next().unwrap_or("")))
        .unwrap_or_default();
    if method == "GET"
        && let Some(query) = target.strip_prefix(&format!("{databases}?"))
    {
        // D1's name filter is a search: it finds other names too.
        let asked = query.strip_prefix("name=").unwrap_or("");
        return Ok(json!([
            {"uuid": "01a15879-0000-7000-8000-0000000000aa", "name": format!("{asked}-staging")},
            {"uuid": UUID, "name": name},
        ]));
    }
    if method == "POST" && target == format!("{databases}/{UUID}/raw") {
        let body: Json = serde_json::from_slice(body)
            .map_err(|err| ("400 Bad Request", 7400, err.to_string()))?;
        return raw(path, &body).map_err(|message| ("400 Bad Request", 7500, message));
    }
    Err((
        "404 Not Found",
        7404,
        format!("no route for {method} {target}"),
    ))
}

/// D1's `/raw`: a batch, as one transaction.
fn raw(path: &str, body: &Json) -> Result<Json, String> {
    let batch = body["batch"]
        .as_array()
        .ok_or("this stand-in takes a batch")?;
    for statement in batch {
        let sql = statement["sql"].as_str().unwrap_or("");
        let params = statement["params"].as_array().cloned().unwrap_or_default();
        let named = params
            .iter()
            .any(|param| param.as_str().is_some_and(|text| text.starts_with("_cf_")));
        let temp = sql.to_ascii_uppercase().contains("CREATE TEMP");
        if sql.contains("\"_cf_") || named || temp {
            return Err(format!("not authorized: SQLITE_AUTH: {sql}"));
        }
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    // On for the whole batch: inside the transaction below, a pragma turning foreign keys off
    // has no effect, as on D1.
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(|err| err.to_string())?;
    let sql = |text: &str| {
        connection
            .execute_batch(text)
            .map_err(|err| err.to_string())
    };
    sql("BEGIN")?;
    let results = batch
        .iter()
        .map(|each| {
            rows(
                &connection,
                each["sql"].as_str().unwrap_or(""),
                &each["params"],
            )
        })
        .collect::<Result<Vec<Json>, String>>();
    sql(if results.is_ok() {
        "COMMIT"
    } else {
        "ROLLBACK"
    })?;
    Ok(Json::Array(
        results?
            .into_iter()
            .map(|rows| json!({"success": true, "results": {"rows": rows}, "meta": {}}))
            .collect(),
    ))
}

/// Runs one statement with D1's text-only parameters, and returns the rows it selected or
/// returned.
fn rows(connection: &Connection, sql: &str, params: &Json) -> Result<Json, String> {
    let error = |err: rusqlite::Error| err.to_string();
    let params = params
        .as_array()
        .map_or(Ok(Vec::new()), |params| params.iter().map(param).collect())?;
    let mut statement = connection.prepare(sql).map_err(error)?;
    let width = statement.column_count();
    let mut rows = Vec::new();
    let mut found = statement.query(params_from_iter(params)).map_err(error)?;
    while let Some(row) = found.next().map_err(error)? {
        rows.push(Json::Array(
            (0..width)
                .map(|i| row.get::<_, Value>(i).map(emit).map_err(error))
                .collect::<Result<Vec<Json>, String>>()?,
        ));
    }
    Ok(Json::Array(rows))
}

/// D1 binds every parameter as text.
fn param(value: &Json) -> Result<Value, String> {
    value
        .as_str()
        .map(|text| Value::Text(text.to_owned()))
        .ok_or_else(|| format!("D1 takes text parameters only, not {value}"))
}

/// The largest integer a JSON number carries exactly.
const JSON_SAFE: i64 = (1 << 53) - 1;

/// D1 answers with plain JSON numbers, integers beyond 2^53 rounded.
#[allow(clippy::cast_precision_loss)]
fn emit(value: Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Integer(integer) if (-JSON_SAFE..=JSON_SAFE).contains(&integer) => json!(integer),
        Value::Integer(integer) => json!(integer as f64),
        Value::Real(real) => json!(real),
        Value::Text(text) => Json::String(text),
        Value::Blob(bytes) => json!(bytes),
    }
}
