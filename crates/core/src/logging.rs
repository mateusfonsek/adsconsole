//! Logs com rotação diária e mascaramento de tokens e secrets.

use crate::CoreError;
use regex::Regex;
use std::borrow::Cow;
use std::io::{self, Write};
use std::path::Path;
use std::sync::LazyLock;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

const KEYS: &str = "access_token|refresh_token|client_secret|app_secret|code_verifier|code|fb_exchange_token|input_token";

/// `"chave":"valor"` (JSON) e `chave: "valor"` (Debug de struct). Aceita `\"` dentro do valor.
static JSON_KV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)((?:^|\W)"?(?:{KEYS})"?\s*:\s*")(?:[^"\\]|\\.)*"#
    ))
    .unwrap()
});
/// Mesmo formato, mas escapado por Debug: `\"chave\":\"valor\"`.
static JSON_KV_ESCAPED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)((?:^|\W)(?:\\")?(?:{KEYS})(?:\\")?\s*:\s*\\")(?:[^\\]|\\[^"])*"#
    ))
    .unwrap()
});
/// `chave=valor` (query/form), com ou sem aspas. A chave só casa inteira (`error_code` não conta).
static FORM_KV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"(?i)((?:^|\W)(?:{KEYS})=)(?:"[^"]*"|'[^']*'|\\"(?:[^\\]|\\[^"])*\\"|[^&\s"']+)"#
    ))
    .unwrap()
});
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\bBearer\s+)[A-Za-z0-9._\-/+=~]+").unwrap());
static BARE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:EAA|ya29\.|1//|GOCSPX-)[A-Za-z0-9._\-/]*").unwrap());

fn apply<'a>(re: &Regex, s: Cow<'a, str>, rep: &str) -> Cow<'a, str> {
    match re.replace_all(&s, rep) {
        Cow::Borrowed(_) => s,
        Cow::Owned(o) => Cow::Owned(o),
    }
}

/// Troca por `***` o valor de tokens e secrets conhecidos; o rótulo fica.
pub fn redact(s: &str) -> Cow<'_, str> {
    let out = apply(&JSON_KV_ESCAPED, Cow::Borrowed(s), "${1}***");
    let out = apply(&JSON_KV, out, "${1}***");
    let out = apply(&FORM_KV, out, "${1}***");
    let out = apply(&BEARER, out, "${1}***");
    apply(&BARE_TOKEN, out, "***")
}

/// Mantém o worker de escrita vivo; ao ser descartado, descarrega o buffer.
pub struct LogGuard {
    _guard: WorkerGuard,
}

#[derive(Clone)]
struct RedactingMakeWriter {
    inner: NonBlocking,
}

struct RedactingWriter {
    inner: NonBlocking,
}

impl Write for RedactingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.inner.write_all(redact(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<'a> MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.clone(),
        }
    }
}

/// Instala o subscriber global (uma vez por processo). Filtro via `RUST_LOG`, padrão `info`.
pub fn init(log_dir: &Path) -> Result<LogGuard, CoreError> {
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("adsconsole")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir)
        .map_err(|e| CoreError::Config(format!("Não foi possível abrir o log: {e}")))?;
    let (inner, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(RedactingMakeWriter { inner })
        .try_init()
        .map_err(|e| CoreError::Config(format!("Log já inicializado: {e}")))?;
    Ok(LogGuard { _guard: guard })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn masks_known_secrets() {
        let cases = [
            ("Authorization: Bearer abc.def", "Authorization: Bearer ***"),
            (
                "GET /oauth?client_id=1&client_secret=s3cr3t&x=1",
                "GET /oauth?client_id=1&client_secret=***&x=1",
            ),
            (
                r#"{"access_token":"EAAB123","expires_in":5}"#,
                r#"{"access_token":"***","expires_in":5}"#,
            ),
            ("token EAABsomething123 ok", "token *** ok"),
            ("secret GOCSPX-abc_DEF-1", "secret ***"),
            ("rt 1//0gAbC-d", "rt ***"),
            ("code=xyz&error_code=7", "code=***&error_code=7"),
            (r#"refresh_token="abc123""#, "refresh_token=***"),
            (
                r#"{\"client_secret\":\"s3\"}"#,
                r#"{\"client_secret\":\"***\"}"#,
            ),
            (
                r#"{"refresh_token":"a\"b","x":1}"#,
                r#"{"refresh_token":"***","x":1}"#,
            ),
            (
                r#"Req { access_token: "abc", id: 1 }"#,
                r#"Req { access_token: "***", id: 1 }"#,
            ),
        ];
        for (i, o) in cases {
            assert_eq!(redact(i), o, "entrada: {i}");
        }
    }

    #[test]
    fn leaves_plain_text_untouched() {
        assert!(matches!(redact("campanha 123 pausada"), Cow::Borrowed(_)));
    }
}
