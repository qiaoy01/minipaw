use std::collections::{BTreeMap, VecDeque};

use crate::types::{now_epoch_secs, Task, TaskId, TaskStatus, ToolResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndexEntry {
    pub id: String,
    pub task_id: Option<TaskId>,
    pub kind: String,
    pub summary: String,
    pub bytes: usize,
    pub score: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDetail {
    pub id: String,
    pub body: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgressiveMemory {
    pub index: Vec<MemoryIndexEntry>,
    pub details: Vec<MemoryDetail>,
}

impl ProgressiveMemory {
    pub fn render(&self) -> String {
        if self.index.is_empty() && self.details.is_empty() {
            return "Memory: none".to_owned();
        }

        let mut out = String::from("Memory index:\n");
        for entry in &self.index {
            out.push_str(&format!(
                "- {} [{} score={}] {}\n",
                entry.id, entry.kind, entry.score, entry.summary
            ));
        }

        if !self.details.is_empty() {
            out.push_str("Selected memory details:\n");
            for detail in &self.details {
                out.push_str(&format!("- {}: {}\n", detail.id, detail.body));
            }
        }
        out
    }
}

pub trait MemoryStore {
    fn create_task(&mut self, title: &str) -> Task;
    fn update_task_status(&mut self, id: TaskId, status: TaskStatus);
    fn get_task(&self, id: TaskId) -> Option<Task>;
    fn list_tasks(&self) -> Vec<Task>;
    fn append_message(&mut self, task_id: TaskId, role: &str, body: &str);
    fn append_tool_result(&mut self, task_id: TaskId, result: &ToolResult);
    fn set_fact(&mut self, key: &str, value: &str);
    fn get_fact(&self, key: &str) -> Option<String>;
    fn memory_index(&self, query: &str, limit: usize) -> Vec<MemoryIndexEntry>;
    fn memory_detail(&self, id: &str, max_bytes: usize) -> Option<MemoryDetail>;
    fn task_context(&self, task_id: TaskId, max_bytes: usize) -> String;

    fn progressive_memory(
        &self,
        query: &str,
        index_limit: usize,
        detail_limit: usize,
        detail_bytes: usize,
    ) -> ProgressiveMemory {
        let index = self.memory_index(query, index_limit);
        let mut details = Vec::new();
        for entry in index.iter().take(detail_limit) {
            if let Some(detail) = self.memory_detail(&entry.id, detail_bytes) {
                details.push(detail);
            }
        }
        ProgressiveMemory { index, details }
    }
}

#[derive(Debug, Clone)]
struct MessageRecord {
    task_id: TaskId,
    role: String,
    body: String,
}

#[derive(Debug, Clone)]
struct ToolRecord {
    task_id: TaskId,
    result: ToolResult,
}

#[derive(Debug)]
pub struct InMemoryStore {
    next_task_id: u64,
    max_records: usize,
    tasks: BTreeMap<u64, Task>,
    messages: VecDeque<MessageRecord>,
    tool_results: VecDeque<ToolRecord>,
    facts: BTreeMap<String, String>,
}

impl InMemoryStore {
    pub fn new(max_records: usize) -> Self {
        Self {
            next_task_id: 1,
            max_records,
            tasks: BTreeMap::new(),
            messages: VecDeque::new(),
            tool_results: VecDeque::new(),
            facts: BTreeMap::new(),
        }
    }

    fn trim(&mut self) {
        while self.messages.len() > self.max_records {
            self.messages.pop_front();
        }
        while self.tool_results.len() > self.max_records {
            self.tool_results.pop_front();
        }
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn tool_result_count(&self) -> usize {
        self.tool_results.len()
    }

    pub fn last_message(&self) -> Option<(TaskId, &str, &str)> {
        self.messages
            .back()
            .map(|record| (record.task_id, record.role.as_str(), record.body.as_str()))
    }

    pub fn last_tool_result(&self) -> Option<(TaskId, &ToolResult)> {
        self.tool_results
            .back()
            .map(|record| (record.task_id, &record.result))
    }
}

impl MemoryStore for InMemoryStore {
    fn create_task(&mut self, title: &str) -> Task {
        let now = now_epoch_secs();
        let task = Task {
            id: TaskId(self.next_task_id),
            title: title.trim().to_owned(),
            status: TaskStatus::New,
            created_at: now,
            updated_at: now,
        };
        self.next_task_id += 1;
        self.tasks.insert(task.id.0, task.clone());
        task
    }

    fn update_task_status(&mut self, id: TaskId, status: TaskStatus) {
        if let Some(task) = self.tasks.get_mut(&id.0) {
            task.status = status;
            task.updated_at = now_epoch_secs();
        }
    }

    fn get_task(&self, id: TaskId) -> Option<Task> {
        self.tasks.get(&id.0).cloned()
    }

    fn list_tasks(&self) -> Vec<Task> {
        self.tasks.values().cloned().collect()
    }

    fn append_message(&mut self, task_id: TaskId, role: &str, body: &str) {
        self.messages.push_back(MessageRecord {
            task_id,
            role: role.to_owned(),
            body: body.to_owned(),
        });
        self.trim();
    }

    fn append_tool_result(&mut self, task_id: TaskId, result: &ToolResult) {
        self.tool_results.push_back(ToolRecord {
            task_id,
            result: result.clone(),
        });
        self.trim();
    }

    fn set_fact(&mut self, key: &str, value: &str) {
        self.facts.insert(key.to_owned(), value.to_owned());
    }

    fn get_fact(&self, key: &str) -> Option<String> {
        self.facts.get(key).cloned()
    }

    fn memory_index(&self, query: &str, limit: usize) -> Vec<MemoryIndexEntry> {
        let mut entries = Vec::new();

        for task in self.tasks.values() {
            entries.push(MemoryIndexEntry {
                id: format!("task:{}", task.id.0),
                task_id: Some(task.id),
                kind: "task".to_owned(),
                summary: cap_chars(&format!("{} [{}]", task.title, task.status), 160),
                bytes: task.title.len(),
                score: relevance(query, &task.title),
            });
        }

        for (index, message) in self.messages.iter().enumerate() {
            entries.push(MemoryIndexEntry {
                id: format!("msg:{}:{index}", message.task_id.0),
                task_id: Some(message.task_id),
                kind: format!("message/{}", message.role),
                summary: cap_chars(&message.body, 160),
                bytes: message.body.len(),
                score: relevance(query, &message.body),
            });
        }

        for (index, tool) in self.tool_results.iter().enumerate() {
            entries.push(MemoryIndexEntry {
                id: format!("tool:{}:{index}", tool.task_id.0),
                task_id: Some(tool.task_id),
                kind: format!("tool/{}", tool.result.name),
                summary: cap_chars(&tool.result.output, 160),
                bytes: tool.result.output.len(),
                score: relevance(query, &tool.result.output),
            });
        }

        for (key, value) in &self.facts {
            entries.push(MemoryIndexEntry {
                id: format!("fact:{key}"),
                task_id: None,
                kind: "fact".to_owned(),
                summary: cap_chars(&format!("{key}={value}"), 160),
                bytes: value.len(),
                score: relevance(query, &format!("{key} {value}")),
            });
        }

        entries.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.id.cmp(&left.id))
        });
        entries.truncate(limit);
        entries
    }

    fn task_context(&self, task_id: TaskId, max_bytes: usize) -> String {
        let parts: Vec<String> = self
            .messages
            .iter()
            .filter(|m| m.task_id == task_id)
            .map(|m| format!("{}: {}", m.role, m.body))
            .collect();
        cap_bytes(&parts.join("\n"), max_bytes)
    }

    fn memory_detail(&self, id: &str, max_bytes: usize) -> Option<MemoryDetail> {
        if let Some(raw) = id.strip_prefix("task:") {
            let task_id = raw.parse::<u64>().ok()?;
            let task = self.tasks.get(&task_id)?;
            return Some(MemoryDetail {
                id: id.to_owned(),
                body: cap_bytes(
                    &format!(
                        "task={} status={} created={} updated={} title={}",
                        task.id, task.status, task.created_at, task.updated_at, task.title
                    ),
                    max_bytes,
                ),
            });
        }

        if let Some(rest) = id.strip_prefix("msg:") {
            let (_, raw_index) = rest.split_once(':')?;
            let index = raw_index.parse::<usize>().ok()?;
            let message = self.messages.get(index)?;
            return Some(MemoryDetail {
                id: id.to_owned(),
                body: cap_bytes(
                    &format!("{} {}: {}", message.task_id, message.role, message.body),
                    max_bytes,
                ),
            });
        }

        if let Some(rest) = id.strip_prefix("tool:") {
            let (_, raw_index) = rest.split_once(':')?;
            let index = raw_index.parse::<usize>().ok()?;
            let tool = self.tool_results.get(index)?;
            return Some(MemoryDetail {
                id: id.to_owned(),
                body: cap_bytes(
                    &format!(
                        "{} {} ok={}: {}",
                        tool.task_id, tool.result.name, tool.result.ok, tool.result.output
                    ),
                    max_bytes,
                ),
            });
        }

        if let Some(key) = id.strip_prefix("fact:") {
            let value = self.facts.get(key)?;
            return Some(MemoryDetail {
                id: id.to_owned(),
                body: cap_bytes(&format!("{key}={value}"), max_bytes),
            });
        }

        None
    }
}

fn relevance(query: &str, text: &str) -> u16 {
    let query = query.to_ascii_lowercase();
    let text = text.to_ascii_lowercase();
    let mut score = 0u16;
    for term in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 3)
    {
        if text.contains(term) {
            score = score.saturating_add(10);
        }
    }
    score.saturating_add(if score == 0 { 1 } else { 0 })
}

fn cap_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in text.chars().take(max_chars) {
        out.push(ch);
    }
    if out.len() < text.len() {
        out.push_str("...");
    }
    out.replace('\n', " ")
}

fn cap_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressive_memory_loads_index_then_relevant_details() {
        let mut store = InMemoryStore::new(16);
        let task = store.create_task("remember sqlite setup for durable memory");
        store.append_message(task.id, "user", "sqlite needs libsqlite3-dev on Debian");
        store.append_message(task.id, "assistant", "install libsqlite3-dev and build");
        store.set_fact("model", "qwen9b");

        let memory = store.progressive_memory("sqlite durable memory", 4, 2, 64);

        assert!(!memory.index.is_empty());
        assert!(memory.index[0].summary.contains("sqlite"));
        assert!(memory.details.len() <= 2);
        assert!(memory.render().contains("Memory index:"));
        assert!(memory.render().contains("Selected memory details:"));
    }

    #[test]
    fn memory_detail_respects_byte_budget() {
        let mut store = InMemoryStore::new(4);
        let task = store.create_task("long task");
        store.append_message(task.id, "assistant", "abcdefghijklmnopqrstuvwxyz");

        let detail = store.memory_detail("msg:1:0", 8).unwrap();

        assert!(detail.body.len() <= 11);
        assert!(detail.body.ends_with("..."));
    }
}

#[cfg(feature = "sqlite")]
pub mod sqlite {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};
    use std::path::Path;
    use std::ptr;

    use crate::memory::{
        cap_bytes, cap_chars, relevance, MemoryDetail, MemoryIndexEntry, MemoryStore,
    };
    use crate::types::{now_epoch_secs, Task, TaskId, TaskStatus, ToolResult};

    const SQLITE_OK: c_int = 0;
    const SQLITE_ROW: c_int = 100;

    #[repr(C)]
    struct sqlite3 {
        _private: [u8; 0],
    }

    #[repr(C)]
    struct sqlite3_stmt {
        _private: [u8; 0],
    }

    #[link(name = "sqlite3")]
    unsafe extern "C" {
        fn sqlite3_open(filename: *const c_char, db: *mut *mut sqlite3) -> c_int;
        fn sqlite3_close(db: *mut sqlite3) -> c_int;
        fn sqlite3_exec(
            db: *mut sqlite3,
            sql: *const c_char,
            callback: Option<
                unsafe extern "C" fn(
                    *mut std::ffi::c_void,
                    c_int,
                    *mut *mut c_char,
                    *mut *mut c_char,
                ) -> c_int,
            >,
            arg: *mut std::ffi::c_void,
            errmsg: *mut *mut c_char,
        ) -> c_int;
        fn sqlite3_free(ptr: *mut std::ffi::c_void);
        fn sqlite3_prepare_v2(
            db: *mut sqlite3,
            sql: *const c_char,
            nbyte: c_int,
            stmt: *mut *mut sqlite3_stmt,
            tail: *mut *const c_char,
        ) -> c_int;
        fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
        fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
        fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, col: c_int) -> i64;
        fn sqlite3_column_text(stmt: *mut sqlite3_stmt, col: c_int) -> *const c_char;
    }

    pub struct SqliteStore {
        db: *mut sqlite3,
        next_task_id: u64,
    }

    impl SqliteStore {
        pub fn open(path: &Path) -> Result<Self, String> {
            let filename = CString::new(path.to_string_lossy().as_bytes())
                .map_err(|_| "sqlite path contains NUL byte".to_owned())?;
            let mut db = ptr::null_mut();
            let rc = unsafe { sqlite3_open(filename.as_ptr(), &mut db) };
            if rc != SQLITE_OK {
                return Err(format!("sqlite open failed with code {rc}"));
            }

            let mut store = Self {
                db,
                next_task_id: 1,
            };
            store.init_schema()?;
            store.next_task_id = store.max_task_id()? + 1;
            Ok(store)
        }

        fn init_schema(&mut self) -> Result<(), String> {
            self.exec(
                "CREATE TABLE IF NOT EXISTS tasks(
                    id INTEGER PRIMARY KEY,
                    status TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    title TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS messages(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id INTEGER NOT NULL,
                    role TEXT NOT NULL,
                    body TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS tool_runs(
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    task_id INTEGER NOT NULL,
                    tool TEXT NOT NULL,
                    ok INTEGER NOT NULL,
                    output TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS facts(
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );",
            )
        }

        fn max_task_id(&self) -> Result<u64, String> {
            let rows = self.query_tasks("SELECT id, status, created_at, updated_at, title FROM tasks ORDER BY id DESC LIMIT 1")?;
            Ok(rows.first().map(|task| task.id.0).unwrap_or(0))
        }

        fn exec(&mut self, sql: &str) -> Result<(), String> {
            let sql = CString::new(sql).map_err(|_| "sql contains NUL byte".to_owned())?;
            let mut err = ptr::null_mut();
            let rc =
                unsafe { sqlite3_exec(self.db, sql.as_ptr(), None, ptr::null_mut(), &mut err) };
            if rc == SQLITE_OK {
                return Ok(());
            }

            let message = if err.is_null() {
                format!("sqlite exec failed with code {rc}")
            } else {
                let text = unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() };
                unsafe { sqlite3_free(err.cast()) };
                text
            };
            Err(message)
        }

        fn query_tasks(&self, sql: &str) -> Result<Vec<Task>, String> {
            let sql = CString::new(sql).map_err(|_| "sql contains NUL byte".to_owned())?;
            let mut stmt = ptr::null_mut();
            let rc = unsafe {
                sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
            };
            if rc != SQLITE_OK {
                return Err(format!("sqlite prepare failed with code {rc}"));
            }

            let mut tasks = Vec::new();
            loop {
                let step = unsafe { sqlite3_step(stmt) };
                if step != SQLITE_ROW {
                    break;
                }
                let id = unsafe { sqlite3_column_int64(stmt, 0) as u64 };
                let status = status_from_db(column_text(stmt, 1).as_deref().unwrap_or("new"));
                let created_at = unsafe { sqlite3_column_int64(stmt, 2) as u64 };
                let updated_at = unsafe { sqlite3_column_int64(stmt, 3) as u64 };
                let title = column_text(stmt, 4).unwrap_or_default();
                tasks.push(Task {
                    id: TaskId(id),
                    title,
                    status,
                    created_at,
                    updated_at,
                });
            }
            unsafe { sqlite3_finalize(stmt) };
            Ok(tasks)
        }

        fn query_text(&self, sql: &str) -> Option<String> {
            let sql = CString::new(sql).ok()?;
            let mut stmt = ptr::null_mut();
            let rc = unsafe {
                sqlite3_prepare_v2(self.db, sql.as_ptr(), -1, &mut stmt, ptr::null_mut())
            };
            if rc != SQLITE_OK {
                return None;
            }
            let value = if unsafe { sqlite3_step(stmt) } == SQLITE_ROW {
                column_text(stmt, 0)
            } else {
                None
            };
            unsafe { sqlite3_finalize(stmt) };
            value
        }
    }

    impl MemoryStore for SqliteStore {
        fn create_task(&mut self, title: &str) -> Task {
            let now = now_epoch_secs();
            let task = Task {
                id: TaskId(self.next_task_id),
                title: title.trim().to_owned(),
                status: TaskStatus::New,
                created_at: now,
                updated_at: now,
            };
            self.next_task_id += 1;
            let _ = self.exec(&format!(
                "INSERT INTO tasks(id, status, created_at, updated_at, title)
                 VALUES({}, 'new', {}, {}, {});",
                task.id.0,
                task.created_at,
                task.updated_at,
                sql_quote(&task.title)
            ));
            task
        }

        fn update_task_status(&mut self, id: TaskId, status: TaskStatus) {
            let _ = self.exec(&format!(
                "UPDATE tasks SET status={}, updated_at={} WHERE id={};",
                sql_quote(&status.to_string()),
                now_epoch_secs(),
                id.0
            ));
        }

        fn get_task(&self, id: TaskId) -> Option<Task> {
            self.query_tasks(&format!(
                "SELECT id, status, created_at, updated_at, title FROM tasks WHERE id={} LIMIT 1",
                id.0
            ))
            .ok()
            .and_then(|mut tasks| tasks.pop())
        }

        fn list_tasks(&self) -> Vec<Task> {
            self.query_tasks(
                "SELECT id, status, created_at, updated_at, title FROM tasks ORDER BY id ASC",
            )
            .unwrap_or_default()
        }

        fn append_message(&mut self, task_id: TaskId, role: &str, body: &str) {
            let _ = self.exec(&format!(
                "INSERT INTO messages(task_id, role, body, created_at) VALUES({}, {}, {}, {});",
                task_id.0,
                sql_quote(role),
                sql_quote(body),
                now_epoch_secs()
            ));
        }

        fn append_tool_result(&mut self, task_id: TaskId, result: &ToolResult) {
            let _ = self.exec(&format!(
                "INSERT INTO tool_runs(task_id, tool, ok, output, created_at)
                 VALUES({}, {}, {}, {}, {});",
                task_id.0,
                sql_quote(&result.name),
                if result.ok { 1 } else { 0 },
                sql_quote(&result.output),
                now_epoch_secs()
            ));
        }

        fn set_fact(&mut self, key: &str, value: &str) {
            let _ = self.exec(&format!(
                "INSERT INTO facts(key, value, updated_at) VALUES({}, {}, {})
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at;",
                sql_quote(key),
                sql_quote(value),
                now_epoch_secs()
            ));
        }

        fn get_fact(&self, key: &str) -> Option<String> {
            self.query_text(&format!(
                "SELECT value FROM facts WHERE key={} LIMIT 1",
                sql_quote(key)
            ))
        }

        fn memory_index(&self, query: &str, limit: usize) -> Vec<MemoryIndexEntry> {
            let mut entries = Vec::new();
            for task in self.list_tasks() {
                entries.push(MemoryIndexEntry {
                    id: format!("task:{}", task.id.0),
                    task_id: Some(task.id),
                    kind: "task".to_owned(),
                    summary: cap_chars(&format!("{} [{}]", task.title, task.status), 160),
                    bytes: task.title.len(),
                    score: relevance(query, &task.title),
                });
            }

            entries.sort_by(|left, right| {
                right
                    .score
                    .cmp(&left.score)
                    .then_with(|| right.id.cmp(&left.id))
            });
            entries.truncate(limit);
            entries
        }

        fn memory_detail(&self, id: &str, max_bytes: usize) -> Option<MemoryDetail> {
            let raw = id.strip_prefix("task:")?;
            let task = self.get_task(TaskId(raw.parse::<u64>().ok()?))?;
            Some(MemoryDetail {
                id: id.to_owned(),
                body: cap_bytes(
                    &format!(
                        "task={} status={} created={} updated={} title={}",
                        task.id, task.status, task.created_at, task.updated_at, task.title
                    ),
                    max_bytes,
                ),
            })
        }

        fn task_context(&self, task_id: TaskId, max_bytes: usize) -> String {
            let rows = self.query_message_rows(task_id);
            let text = rows
                .into_iter()
                .map(|(role, body)| format!("{role}: {body}"))
                .collect::<Vec<_>>()
                .join("\n");
            cap_bytes(&text, max_bytes)
        }
    }

    impl SqliteStore {
        fn query_message_rows(&self, task_id: TaskId) -> Vec<(String, String)> {
            let sql = format!(
                "SELECT role, body FROM messages WHERE task_id={} ORDER BY id ASC",
                task_id.0
            );
            let Ok(cstring) = CString::new(sql) else {
                return Vec::new();
            };
            let mut stmt = ptr::null_mut();
            let rc = unsafe {
                sqlite3_prepare_v2(self.db, cstring.as_ptr(), -1, &mut stmt, ptr::null_mut())
            };
            if rc != SQLITE_OK {
                return Vec::new();
            }
            let mut rows = Vec::new();
            loop {
                if unsafe { sqlite3_step(stmt) } != SQLITE_ROW {
                    break;
                }
                let role = column_text(stmt, 0).unwrap_or_default();
                let body = column_text(stmt, 1).unwrap_or_default();
                rows.push((role, body));
            }
            unsafe { sqlite3_finalize(stmt) };
            rows
        }
    }

    impl Drop for SqliteStore {
        fn drop(&mut self) {
            if !self.db.is_null() {
                unsafe {
                    sqlite3_close(self.db);
                }
            }
        }
    }

    fn column_text(stmt: *mut sqlite3_stmt, col: c_int) -> Option<String> {
        let ptr = unsafe { sqlite3_column_text(stmt, col) };
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() })
        }
    }

    fn sql_quote(value: &str) -> String {
        let escaped = value.replace('\0', "").replace('\'', "''");
        format!("'{escaped}'")
    }

    fn status_from_db(value: &str) -> TaskStatus {
        match value {
            "running" => TaskStatus::Running,
            "waiting" => TaskStatus::Waiting,
            "done" => TaskStatus::Done,
            "failed" => TaskStatus::Failed,
            _ => TaskStatus::New,
        }
    }
}
