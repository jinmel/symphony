use std::error::Error;

pub type BoxError = Box<dyn Error + Send + Sync + 'static>;

pub fn truncate_for_log(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        value.to_owned()
    } else {
        format!("{}...<truncated>", &value[..max_bytes])
    }
}
