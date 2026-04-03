/// Escape `\`, `%`, and `_` for embedding a literal prefix inside an SQLite `LIKE` pattern when using `ESCAPE '\\'`.
pub(crate) fn escape_sqlite_like_fragment(s: &str) -> String {
    let mut out = String::with_capacity(s.len().saturating_add(8));
    for ch in s.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}
