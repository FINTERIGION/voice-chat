use std::collections::HashSet;

use serde::Deserialize;

use crate::llm::flash::FlashClient;
use crate::store::memory::{self as store, Memory};
use crate::store::message::Message;

/// Roughly 800 tokens' worth of characters for the injected `[长期记忆]`
/// block. CJK text runs close to 1-1.5 chars/token, so this is a generous
/// but conservative proxy without needing a real tokenizer.
const INJECT_BUDGET_CHARS: usize = 700;
const MAX_FACTS_PER_CHARACTER: usize = 30;
/// Enough of a conversation to tell what it is about. A title only ever
/// describes what the conversation opened with, so feeding the whole
/// transcript would cost tokens without changing the answer.
const TITLE_CONTEXT_MESSAGES: usize = 12;
/// Hard cap on the stored title, in characters. The model is asked for
/// something far shorter; this only catches a model that ignores the
/// instruction, so one bad response can't put a paragraph in the sidebar.
const MAX_TITLE_CHARS: usize = 40;

#[derive(Debug)]
pub struct SummaryResult {
    pub summary: String,
    /// Facts this conversation brought up that weren't known yet. The
    /// summarizer is shown the known ones and told not to repeat them;
    /// `store_summary` drops whatever exact repeat gets through anyway.
    pub facts: Vec<String>,
    /// Known facts this conversation overturned — a new name to be called,
    /// a changed preference — reworded in place rather than left standing
    /// next to their replacement.
    pub revised_facts: Vec<FactRevision>,
    /// Threads still worth raising next time. Replaces the previous set.
    pub open_loops: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct FactRevision {
    /// Into the `known_facts` the summarizer was shown.
    pub index: usize,
    pub text: String,
}

/// Summarizes one conversation's transcript into a rolling summary plus
/// discrete facts, folding in whatever summary already existed so the
/// result stays continuous across many short conversations rather than
/// starting fresh each time.
///
/// `known_facts` are the character's stored facts, shown to the model so it
/// adds only what is new and revises what changed; `FactRevision::index`
/// points back into this slice.
pub async fn summarize_conversation(
    client: &FlashClient,
    character_name: &str,
    previous_summary: Option<&str>,
    known_facts: &[String],
    previous_open_loops: &[String],
    messages: &[Message],
) -> Result<SummaryResult, String> {
    if messages.is_empty() {
        return Err(crate::tr!(
            "Nothing in this conversation to summarize",
            "没有可摘要的对话内容"
        )
        .to_string());
    }

    let transcript = messages
        .iter()
        .map(|m| {
            let speaker = if m.role == "user" {
                "用户"
            } else {
                character_name
            };
            format!("{speaker}: {}", m.text)
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Unfinished threads are kept out of the summary: they are injected as
    // their own lines anyway, and a thread stated twice both spends the
    // summary's 200 characters and invites the model to raise it twice.
    let system = "你是语音陪聊应用的记忆整理助手。基于本轮对话记录（可能附带历史摘要、已知事实和未完话题），\
        输出严格的 JSON，不要有任何其他文字：\
        {\"summary\": \"合并历史摘要与本轮内容后的滚动摘要，不超过200字，保留称呼、相处节奏和对方在意的事\", \
        \"facts\": [\"本轮新出现的稳定事实，如偏好、称呼、约定，每条不超过40字\"], \
        \"revised_facts\": [{\"n\": 已知事实的编号, \"text\": \"更新后的内容，不超过40字\"}], \
        \"open_loops\": [\"下次可以自然提起的未完话题，每条不超过30字\"]}\
        facts 最多 5 条，open_loops 最多 3 条，没有就给空数组。\
        未完话题只写进 open_loops，summary 里不要重复。\
        facts 只写已知事实里没有的；意思相同、只是说法不同的也算已有，不要再写。\
        已知事实被本轮对话推翻或改变了（比如换了称呼、改了偏好），放进 revised_facts，按编号给出新内容，不要再写进 facts；\
        只是换个说法的不要改。没有就给空数组。\
        open_loops 只留现在还没了结的事。附带的旧话题如果本轮没解决，要保留；已经问过或已经解决的不要留。";

    let mut user = String::new();
    if !known_facts.is_empty() {
        user.push_str("已知事实：\n");
        for (i, fact) in known_facts.iter().enumerate() {
            // 1-based, matching the `n` the model is asked to answer with.
            user.push_str(&format!("{}. {}\n", i + 1, fact.trim()));
        }
        user.push('\n');
    }
    let pending: Vec<&str> = previous_open_loops
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if !pending.is_empty() {
        user.push_str("仍未了结的话题：\n");
        for item in &pending {
            user.push_str("- ");
            user.push_str(item);
            user.push('\n');
        }
        user.push('\n');
    }
    if let Some(prev) = previous_summary {
        if !prev.trim().is_empty() {
            user.push_str("历史摘要：");
            user.push_str(prev.trim());
            user.push_str("\n\n");
        }
    }
    user.push_str("本轮对话：\n");
    user.push_str(&transcript);

    let raw = client.complete_long(system, &user).await?;
    parse_summary(&raw, known_facts.len())
}

/// The summarizer sometimes stops (`finish_reason: stop`) with the outermost
/// `}` never emitted. The rest of the object is well-formed, so the missing
/// closers can be put back, after dropping a dangling `,` a cut between two
/// entries leaves behind. A cut inside a string is left alone: closing it
/// would store text the model did not write.
fn append_missing_closers(input: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escape = false;
    for c in input.chars() {
        if in_string {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop()?;
            }
            _ => {}
        }
    }
    if in_string || stack.is_empty() {
        return None;
    }
    let body = input.trim_end();
    let mut out = body.strip_suffix(',').unwrap_or(body).to_string();
    while let Some(c) = stack.pop() {
        out.push(c);
    }
    Some(out)
}

/// Reads the summarizer's JSON. `known_count` is how many facts it was shown,
/// which bounds the revisions it can make.
fn parse_summary(raw: &str, known_count: usize) -> Result<SummaryResult, String> {
    #[derive(Deserialize)]
    struct Parsed {
        summary: String,
        #[serde(default)]
        facts: Vec<String>,
        /// Kept loose, so one malformed revision is dropped on its own
        /// instead of failing — and losing — the whole summary with it.
        #[serde(default)]
        revised_facts: Vec<serde_json::Value>,
        #[serde(default)]
        open_loops: Vec<String>,
    }
    let trimmed = raw.trim();
    let parsed = serde_json::from_str::<Parsed>(trimmed).or_else(|err| {
        append_missing_closers(trimmed)
            .and_then(|repaired| serde_json::from_str(&repaired).ok())
            .ok_or_else(|| {
                crate::tr!(
                    format!(
                        "Could not parse the summary: {err}; raw: {}",
                        crate::dashscope::snippet(raw)
                    ),
                    format!(
                        "解析摘要失败: {err}; 原始: {}",
                        crate::dashscope::snippet(raw)
                    ),
                )
            })
    })?;

    Ok(SummaryResult {
        summary: parsed.summary,
        facts: parsed.facts,
        revised_facts: parsed
            .revised_facts
            .iter()
            .filter_map(|v| parse_revision(v, known_count))
            .collect(),
        open_loops: parsed.open_loops,
    })
}

/// One `{"n": 2, "text": "…"}` entry, or `None` if it doesn't name a fact
/// the model was actually shown or has nothing to replace it with. `n` is
/// accepted as a number or a numeric string: models write both.
fn parse_revision(value: &serde_json::Value, known_count: usize) -> Option<FactRevision> {
    let n = match value.get("n")? {
        serde_json::Value::Number(n) => n.as_u64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    let text = value.get("text")?.as_str()?.trim();
    let index = usize::try_from(n).ok()?.checked_sub(1)?;
    (index < known_count && !text.is_empty()).then(|| FactRevision {
        index,
        text: text.to_string(),
    })
}

/// Names a conversation for the Chat tab's history list.
///
/// Deliberately a separate, cheap call rather than another field on
/// `summarize_conversation`: naming happens early, while the conversation is
/// still going and there is only a turn or two to go on, whereas
/// summarization happens once at the end over the whole transcript.
pub async fn generate_title(
    client: &FlashClient,
    character_name: &str,
    messages: &[Message],
) -> Result<String, String> {
    let transcript = messages
        .iter()
        .filter(|m| !m.text.trim().is_empty())
        .take(TITLE_CONTEXT_MESSAGES)
        .map(|m| {
            let speaker = if m.role == "user" {
                "用户"
            } else {
                character_name
            };
            format!("{speaker}: {}", m.text)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if transcript.is_empty() {
        return Err(crate::tr!(
            "Nothing in this conversation to name",
            "没有可命名的对话内容"
        )
        .to_string());
    }

    let system = "你是对话标题助手。根据对话记录，起一个概括主题的短标题。\
        要求：中文不超过 12 个字，英文不超过 6 个词；\
        使用与对话内容相同的语言；不要引号、书名号，不要以标点结尾；\
        只输出标题本身，不要任何解释。";

    let raw = client.complete(system, &transcript).await?;
    let title = clean_title(&raw);
    if title.is_empty() {
        return Err(crate::tr!(
            format!(
                "The model returned no usable title; raw: {}",
                crate::dashscope::snippet(&raw)
            ),
            format!(
                "模型没有返回可用的标题; 原始: {}",
                crate::dashscope::snippet(&raw)
            ),
        ));
    }
    Ok(title)
}

/// Reduces a model response to something that fits one sidebar row: the
/// first line only (a model that adds an explanation puts it below), with
/// the quotes it likes to wrap titles in stripped off.
fn clean_title(raw: &str) -> String {
    const WRAPPERS: &[char] = &['"', '\'', '“', '”', '‘', '’', '《', '》', '「', '」'];
    raw.trim()
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches(|c| WRAPPERS.contains(&c))
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Writes a summarization result into storage: replaces the rolling summary
/// row, rewords the facts it revised, adds the new ones, drops repeats, and
/// caps total fact count so memories don't grow unbounded over a long-lived
/// character.
///
/// `known_facts` must be the facts the summarizer was shown, in the same
/// order, since that is what `FactRevision::index` counts into.
pub fn store_summary(
    conn: &rusqlite::Connection,
    character_id: &str,
    known_facts: &[Memory],
    result: &SummaryResult,
) -> Result<(), String> {
    let db = |e: rusqlite::Error| e.to_string();
    store::replace_summary(conn, character_id, &result.summary).map_err(db)?;
    for revision in &result.revised_facts {
        // A fact deleted in the memory manager while the summary was being
        // written makes this a no-op, which is what it should be.
        if let Some(fact) = known_facts.get(revision.index) {
            store::update_content(conn, &fact.id, &revision.text).map_err(db)?;
        }
    }
    for fact in &result.facts {
        let fact = fact.trim();
        if fact.is_empty() {
            continue;
        }
        store::create(conn, character_id, "fact", fact, 0.5).map_err(db)?;
    }
    // Before the cap, so duplicates don't take up places real facts need.
    dedupe_facts(conn, character_id)?;
    store::cap_facts(conn, character_id, MAX_FACTS_PER_CHARACTER).map_err(db)?;
    store::replace_open_loops(conn, character_id, &result.open_loops).map_err(db)?;
    Ok(())
}

/// Deletes each fact that says what a newer one says, give or take spacing,
/// punctuation and letter case — the repeats that get past the summarizer
/// despite it being shown the known facts, plus any stored before it was.
///
/// The newest copy is the one kept, so a fact that comes up again counts as
/// recently confirmed, and moves up in `select_for_injection`.
fn dedupe_facts(conn: &rusqlite::Connection, character_id: &str) -> Result<(), String> {
    // Newest first.
    let memories = store::list(conn, character_id).map_err(|e| e.to_string())?;
    let mut seen = HashSet::new();
    for fact in memories.iter().filter(|m| m.kind == "fact") {
        if !seen.insert(fact_key(&fact.content)) {
            store::delete(conn, &fact.id).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// What two facts must share to count as the same fact: their letters and
/// digits, case-folded. Catches "喜欢乌龙茶" against "喜欢乌龙茶。"; a real
/// paraphrase is the summarizer's job, since only it can tell.
fn fact_key(content: &str) -> String {
    content
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn memory_label(kind: &str) -> &'static str {
    match kind {
        "summary" => "摘要",
        "open_loop" => "未完话题",
        "profile" => "画像",
        _ => "事实",
    }
}

fn memory_line(m: &Memory) -> String {
    format!("{}：{}", memory_label(&m.kind), m.content.trim())
}

/// What one memory adds to the block: its line plus the newline joining it
/// to the next. One newline more than the block ends up holding, which only
/// ever errs toward fitting.
fn line_cost(m: &Memory) -> usize {
    memory_line(m).chars().count() + 1
}

fn concat_memories(memories: &[Memory]) -> String {
    memories
        .iter()
        .map(memory_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Picks what goes into the `[长期记忆]` block, in the order it is written:
/// the summary and every unfinished thread always, then as many facts as the
/// rest of the budget holds, most recently confirmed first.
///
/// Facts are metered by length rather than counted. They run from a few
/// characters to forty, so a fixed number of them either left most of the
/// budget unused or overran it — and an overrun means a compression call
/// before every connect. A fact too long for what is left is skipped rather
/// than ending the list, since a shorter one after it may still fit.
///
/// Sync DB read, kept separate from `build_injection_block` below so
/// callers never hold the `rusqlite::Connection` mutex guard across an
/// `.await` (which would make the enclosing actor future non-`Send` and
/// fail to compile under `tauri::async_runtime::spawn`).
pub fn select_for_injection(
    conn: &rusqlite::Connection,
    character_id: &str,
) -> Result<Vec<Memory>, String> {
    // Newest first, which is the order facts are offered the budget in.
    let memories = store::list(conn, character_id).map_err(|e| e.to_string())?;
    let (facts, mut picked): (Vec<Memory>, Vec<Memory>) = memories
        .into_iter()
        .filter(|m| !m.content.trim().is_empty())
        .partition(|m| m.kind == "fact");
    // Stable, so unfinished threads keep their newest-first order.
    picked.sort_by_key(|m| match m.kind.as_str() {
        "summary" => 0,
        "profile" => 1,
        _ => 2,
    });

    let mut used: usize = picked.iter().map(line_cost).sum();
    for fact in facts {
        let cost = line_cost(&fact);
        if used + cost <= INJECT_BUDGET_CHARS {
            used += cost;
            picked.push(fact);
        }
    }
    Ok(picked)
}

/// Builds the `[长期记忆]` block for a character's next `instructions` from
/// memories already picked by `select_for_injection`. Facts are metered to
/// fit there, so compressing via `flash` is left for the one case they
/// can't help: a summary and threads that alone overrun the budget, which
/// takes hand-lengthened entries from the memory manager.
pub async fn build_injection_block(client: &FlashClient, memories: &[Memory]) -> Option<String> {
    if memories.is_empty() {
        return None;
    }

    let block = concat_memories(memories);
    if block.chars().count() <= INJECT_BUDGET_CHARS {
        return Some(block);
    }

    let system = "请把下面的长期记忆压到300字以内，保留最重要的信息，尤其是未完话题。\
        保留「摘要」「未完话题」「事实」这些前缀。直接输出正文，不要解释、不要加引号。";
    match client.complete(system, &block).await {
        Ok(compressed) => Some(compressed),
        Err(e) => {
            tracing::warn!("memory compression failed, truncating instead: {e}");
            Some(block.chars().take(INJECT_BUDGET_CHARS).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{character, db};

    /// A migrated, empty database in a throwaway file — `db::open` is what
    /// applies the migrations, and it takes a path.
    struct TempDb {
        path: std::path::PathBuf,
        conn: rusqlite::Connection,
    }

    impl TempDb {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("voice-chat-memory-test-{}.db", uuid::Uuid::new_v4()));
            let conn = db::open(&path).expect("open");
            Self { path, conn }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn character(conn: &rusqlite::Connection) -> String {
        character::create(conn, character::CharacterInput::default_new("Nia", "", ""))
            .expect("character")
            .id
    }

    /// Stores a memory dated `minutes_ago`, so ordering doesn't hang on how
    /// finely the clock ticks between two inserts.
    fn remember(
        conn: &rusqlite::Connection,
        character_id: &str,
        kind: &str,
        content: &str,
        minutes_ago: i64,
    ) -> Memory {
        let m = store::create(conn, character_id, kind, content, 0.5).expect("memory");
        let at = (chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339();
        conn.execute(
            "UPDATE memories SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![at, m.id],
        )
        .expect("date");
        Memory {
            updated_at: at,
            ..m
        }
    }

    #[test]
    fn summary_missing_its_closing_brace_still_parses() {
        // Captured from a live `finish_reason: stop` response that ended on
        // `"open_loops": []` with the outer `}` never sent.
        let raw = r#"{"summary": "用户与小柔进行简短寒暄，表示今天过得还可以，但记不清最近有什么开心的小事。小柔回应说日子平平淡淡也没关系。", "facts": [], "revised_facts": [], "open_loops": []"#;
        let result = parse_summary(raw, 0).expect("closer appended");
        assert!(result.summary.contains("小柔"));
        assert!(result.facts.is_empty());
        assert!(result.open_loops.is_empty());

        let nested = r#"{"summary": "用户与Emma闲聊", "facts": ["应用即将发布第二版本"], "revised_facts": [{"n": 1, "text": "用户已发布首个版本，即将发布第二版"}], "open_loops": []"#;
        let nested = parse_summary(nested, 1).expect("nested closer appended");
        assert_eq!(nested.facts, ["应用即将发布第二版本"]);
        assert_eq!(
            nested.revised_facts,
            [FactRevision {
                index: 0,
                text: "用户已发布首个版本，即将发布第二版".into(),
            }]
        );

        let comma = r#"{"summary": "聊了搬家", "facts": ["养了一只猫"], "open_loops": [], "#;
        let comma = parse_summary(comma, 0).expect("dangling comma dropped");
        assert_eq!(comma.facts, ["养了一只猫"]);

        assert!(parse_summary(r#"{"summary": "还没说完"#, 0).is_err());
    }

    #[test]
    fn revisions_must_name_a_fact_that_was_shown() {
        let raw = r#"{"summary": "聊了搬家", "facts": ["养了一只猫"],
            "revised_facts": [
                {"n": 2, "text": "叫他李哥"},
                {"n": "1", "text": " 不喝咖啡了 "},
                {"n": 3, "text": "past the end"},
                {"n": 0, "text": "before the start"},
                {"n": 1, "text": "  "},
                "not an object"
            ],
            "open_loops": []}"#;
        let result = parse_summary(raw, 2).expect("one bad revision doesn't sink the rest");
        assert_eq!(
            result.revised_facts,
            [
                FactRevision {
                    index: 1,
                    text: "叫他李哥".into()
                },
                FactRevision {
                    index: 0,
                    text: "不喝咖啡了".into()
                },
            ]
        );
        assert_eq!(result.facts, ["养了一只猫"]);

        let bare = parse_summary(r#"{"summary": "s", "facts": []}"#, 3).expect("parses");
        assert!(bare.revised_facts.is_empty());
    }

    #[test]
    fn storing_a_summary_revises_in_place_and_drops_repeats() {
        let db = TempDb::new();
        let c = character(&db.conn);
        let tea = remember(&db.conn, &c, "fact", "喜欢乌龙茶", 10);
        let name = remember(&db.conn, &c, "fact", "叫他小李", 20);
        // A repeat stored before the summarizer was shown the known facts.
        remember(&db.conn, &c, "fact", "喜欢 乌龙茶！", 30);

        let result = SummaryResult {
            summary: "聊了搬家".into(),
            facts: vec!["喜欢乌龙茶。".into(), "养了一只猫".into(), " ".into()],
            revised_facts: vec![FactRevision {
                index: 1,
                text: "叫他李哥".into(),
            }],
            open_loops: vec![],
        };
        store_summary(&db.conn, &c, &[tea, name], &result).expect("store");

        let mut facts: Vec<String> = store::list(&db.conn, &c)
            .expect("list")
            .into_iter()
            .filter(|m| m.kind == "fact")
            .map(|m| m.content)
            .collect();
        facts.sort();
        let mut expected = vec!["喜欢乌龙茶。", "养了一只猫", "叫他李哥"];
        expected.sort();
        assert_eq!(
            facts, expected,
            "one copy of the tea, the newest; the name revised, not duplicated"
        );
    }

    #[test]
    fn injection_pins_summary_and_threads_then_fills_with_newest_facts() {
        let db = TempDb::new();
        let c = character(&db.conn);
        let summary = "摘".repeat(200);
        remember(&db.conn, &c, "fact", "oldest fact", 50);
        remember(&db.conn, &c, "open_loop", "面试结果还没问", 40);
        remember(&db.conn, &c, "summary", &summary, 30);
        remember(&db.conn, &c, "fact", "短事实二", 3);
        // Too long for what is left, but the shorter ones around it still go in.
        remember(&db.conn, &c, "fact", &"长".repeat(480), 2);
        remember(&db.conn, &c, "fact", "短事实一", 1);
        remember(&db.conn, &c, "fact", "  ", 0);

        let picked = select_for_injection(&db.conn, &c).expect("select");
        assert_eq!(
            picked
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            [
                summary.as_str(),
                "面试结果还没问",
                "短事实一",
                "短事实二",
                "oldest fact"
            ]
        );
        assert!(concat_memories(&picked).chars().count() <= INJECT_BUDGET_CHARS);
    }
}
