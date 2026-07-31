/// Decode a nullable `egress_label_json` column into an [`EgressLabel`].
///
/// Empty/NULL ⇒ `None` (⇒ unrestricted at the call site). A *present but
/// malformed* label fails the conversion rather than silently degrading to
/// `None` — readers must not turn corruption into an under-restriction
/// (RFC §2.2 fail-closed). Shared by every label-listing query.
pub(crate) fn decode_egress_label_json(
    raw: Option<String>,
) -> rusqlite::Result<Option<autonoetic_types::egress::EgressLabel>> {
    match raw {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                19,
                rusqlite::types::Type::Text,
                e.to_string().into(),
            )
        }),
        _ => Ok(None),
    }
}

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
