//! Cosmonic Task Manager — a small stateful component.
//!
//! Serves a to-do list over HTTP and persists it to the host key-value store
//! (`wasi:keyvalue`), which Cosmonic Desktop provides zero-config. No database
//! and no outbound network: the component starts with no authority and reaches
//! only the two things its Workload grants it — the HTTP trigger and the store.
//!
//! The HTTP handler is implemented directly against `wasi:http@0.2.2` (rather
//! than a framework) so the exported interface version matches exactly what the
//! Desktop ingress binds.

mod bindings {
    wit_bindgen::generate!({ generate_all });
}

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::keyvalue::store;
use serde::{Deserialize, Serialize};

struct Component;

#[derive(Serialize, Deserialize, Clone)]
struct Task {
    id: u64,
    title: String,
    done: bool,
}

const TASKS_KEY: &str = "tasks";
const ID_KEY: &str = "next-id";

fn kv_err<E>(_e: E) -> String {
    "key-value store error".to_string()
}

fn load_tasks() -> Result<Vec<Task>, String> {
    let bucket = store::open("").map_err(kv_err)?;
    match bucket.get(TASKS_KEY).map_err(kv_err)? {
        Some(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        None => Ok(Vec::new()),
    }
}

fn save_tasks(tasks: &[Task]) -> Result<(), String> {
    let bucket = store::open("").map_err(kv_err)?;
    let bytes = serde_json::to_vec(tasks).map_err(|e| e.to_string())?;
    bucket.set(TASKS_KEY, &bytes).map_err(kv_err)
}

// A monotonic id, kept in the store as a little-endian u64. We use plain
// get/set rather than wasi:keyvalue/atomics on purpose: importing the atomics
// interface stops the host from routing the HTTP trigger, and a single-writer
// to-do list doesn't need an atomic increment.
fn next_id() -> Result<u64, String> {
    let bucket = store::open("").map_err(kv_err)?;
    let current = match bucket.get(ID_KEY).map_err(kv_err)? {
        Some(bytes) => {
            let arr: [u8; 8] = bytes.as_slice().try_into().unwrap_or([0; 8]);
            u64::from_le_bytes(arr)
        }
        None => 0,
    };
    let next = current + 1;
    bucket.set(ID_KEY, &next.to_le_bytes()).map_err(kv_err)?;
    Ok(next)
}

fn tasks_json(tasks: &[Task]) -> String {
    serde_json::to_string(tasks).unwrap_or_else(|_| "[]".to_string())
}

fn qparam(query: &str, key: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn respond(out: ResponseOutparam, status: u16, content_type: &str, body_bytes: &[u8]) {
    let headers = Fields::new();
    let _ = headers.set(
        &"content-type".to_string(),
        &[content_type.as_bytes().to_vec()],
    );
    let response = OutgoingResponse::new(headers);
    let _ = response.set_status_code(status);
    let Ok(body) = response.body() else {
        ResponseOutparam::set(out, Err(bindings::wasi::http::types::ErrorCode::InternalError(None)));
        return;
    };
    ResponseOutparam::set(out, Ok(response));
    if let Ok(stream) = body.write() {
        let _ = stream.blocking_write_and_flush(body_bytes);
    }
    let _ = OutgoingBody::finish(body, None);
}

fn json(out: ResponseOutparam, tasks: &[Task]) {
    respond(out, 200, "application/json", tasks_json(tasks).as_bytes());
}

fn handle_api(is_get: bool, path: &str, query: &str) -> Result<Vec<Task>, String> {
    match (is_get, path) {
        (true, "/api/tasks") => load_tasks(),
        (false, "/api/tasks") => {
            let title = qparam(query, "title").unwrap_or_default();
            let title = title.trim();
            if title.is_empty() {
                return load_tasks();
            }
            let id = next_id()?;
            let mut tasks = load_tasks()?;
            tasks.push(Task {
                id,
                title: title.to_string(),
                done: false,
            });
            save_tasks(&tasks)?;
            Ok(tasks)
        }
        (false, "/api/tasks/toggle") => {
            let id = qparam(query, "id").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let mut tasks = load_tasks()?;
            for task in tasks.iter_mut().filter(|t| t.id == id) {
                task.done = !task.done;
            }
            save_tasks(&tasks)?;
            Ok(tasks)
        }
        (false, "/api/tasks/delete") => {
            let id = qparam(query, "id").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let mut tasks = load_tasks()?;
            tasks.retain(|t| t.id != id);
            save_tasks(&tasks)?;
            Ok(tasks)
        }
        _ => Err("__notfound__".to_string()),
    }
}

impl Guest for Component {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let is_get = matches!(request.method(), Method::Get);
        let pq = request
            .path_with_query()
            .unwrap_or_else(|| "/".to_string());
        let (path, query) = match pq.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (pq, String::new()),
        };

        if is_get && path == "/" {
            return respond(response_out, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes());
        }

        match handle_api(is_get, &path, &query) {
            Ok(tasks) => json(response_out, &tasks),
            Err(e) if e == "__notfound__" => {
                respond(response_out, 404, "text/plain", b"Not found\n")
            }
            Err(e) => respond(
                response_out,
                500,
                "text/plain",
                format!("error: {e}\n").as_bytes(),
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Task Manager · Cosmonic</title>
<style>
  :root { color-scheme: light dark; --bg:#0a1830; --card:#12213f; --line:#26365c; --fg:#eaf0fb; --muted:#8fa1c4; --accent:#7c6cff; --done:#4b5d84; }
  * { box-sizing: border-box; }
  body { margin:0; min-height:100vh; font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; background:radial-gradient(1200px 600px at 50% -10%, #16294d, var(--bg)); color:var(--fg); display:flex; justify-content:center; padding:48px 20px; }
  main { width:100%; max-width:560px; }
  header { display:flex; align-items:center; gap:12px; margin-bottom:6px; }
  header .dot { width:12px; height:12px; border-radius:50%; background:var(--accent); box-shadow:0 0 16px var(--accent); }
  h1 { font-size:22px; margin:0; letter-spacing:-0.01em; }
  .sub { color:var(--muted); font-size:14px; margin:0 0 24px 24px; }
  form { display:flex; gap:10px; margin-bottom:20px; }
  input[type=text] { flex:1; padding:12px 14px; border-radius:10px; border:1px solid var(--line); background:var(--card); color:var(--fg); font-size:15px; }
  input[type=text]:focus { outline:none; border-color:var(--accent); }
  button { cursor:pointer; border:none; border-radius:10px; font-size:15px; font-weight:600; }
  .add { padding:12px 18px; background:var(--accent); color:#fff; }
  ul { list-style:none; margin:0; padding:0; display:flex; flex-direction:column; gap:8px; }
  li { display:flex; align-items:center; gap:12px; padding:12px 14px; background:var(--card); border:1px solid var(--line); border-radius:10px; }
  li .title { flex:1; font-size:15px; }
  li.done .title { color:var(--done); text-decoration:line-through; }
  .check { width:22px; height:22px; border-radius:6px; border:1px solid var(--line); background:transparent; color:var(--accent); font-size:14px; display:flex; align-items:center; justify-content:center; }
  li.done .check { background:var(--accent); color:#fff; border-color:var(--accent); }
  .del { background:transparent; color:var(--muted); padding:6px 8px; font-size:18px; line-height:1; }
  .del:hover { color:#ff7a7a; }
  .empty { color:var(--muted); text-align:center; padding:24px; font-size:14px; }
  footer { color:var(--muted); font-size:12px; text-align:center; margin-top:28px; line-height:1.6; }
</style>
</head>
<body>
<main>
  <header><span class="dot"></span><h1>Task Manager</h1></header>
  <p class="sub">A stateful Wasm component. Your tasks live in the host key-value store.</p>
  <form id="add-form">
    <input type="text" id="title" placeholder="Add a task&hellip;" autocomplete="off" autofocus>
    <button class="add" type="submit">Add</button>
  </form>
  <ul id="list"></ul>
  <div class="empty" id="empty" hidden>Nothing yet. Add your first task above.</div>
  <footer>Served by a WebAssembly component running sandboxed on your machine.<br>It can reach only the HTTP trigger and the key-value store &mdash; nothing else.</footer>
</main>
<script>
const list = document.getElementById('list');
const empty = document.getElementById('empty');
function render(tasks) {
  list.innerHTML = '';
  empty.hidden = tasks.length > 0;
  for (const t of tasks) {
    const li = document.createElement('li');
    if (t.done) li.className = 'done';
    const check = document.createElement('button');
    check.className = 'check'; check.textContent = t.done ? '✓' : '';
    check.onclick = () => post('/api/tasks/toggle?id=' + t.id);
    const title = document.createElement('span');
    title.className = 'title'; title.textContent = t.title;
    const del = document.createElement('button');
    del.className = 'del'; del.textContent = '×';
    del.onclick = () => post('/api/tasks/delete?id=' + t.id);
    li.append(check, title, del);
    list.append(li);
  }
}
async function post(url) { render(await (await fetch(url, {method:'POST'})).json()); }
async function load() { render(await (await fetch('/api/tasks')).json()); }
document.getElementById('add-form').onsubmit = async (e) => {
  e.preventDefault();
  const input = document.getElementById('title');
  const title = input.value.trim();
  if (!title) return;
  input.value = '';
  render(await (await fetch('/api/tasks?title=' + encodeURIComponent(title), {method:'POST'})).json());
};
load();
</script>
</body>
</html>
"#;
