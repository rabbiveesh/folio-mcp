//! `{placeholder}` expansion for command arguments and file paths.
//!
//! Supported: `{book}`, `{locator}`, `{page}` (1-based), `{page0}` (0-based),
//! each optionally zero-padded as `{page:04}`. `{{` and `}}` are literal braces.

use anyhow::{Result, bail};

pub struct Vars<'a> {
    pub book: &'a str,
    pub locator: &'a str,
    pub page: Option<u32>,
}

pub fn expand(template: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut spec = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => spec.push(c),
                        None => bail!("unclosed '{{' in template {template:?}"),
                    }
                }
                out.push_str(&substitute(&spec, vars, template)?);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn substitute(spec: &str, vars: &Vars, template: &str) -> Result<String> {
    let (name, width) = match spec.split_once(':') {
        Some((name, fmt)) => {
            let Some(width) = fmt.strip_prefix('0').and_then(|w| w.parse::<usize>().ok()) else {
                bail!("bad format {fmt:?} in template {template:?} (only zero-padding like :04)");
            };
            (name, width)
        }
        None => (spec, 0),
    };
    let page = || match vars.page {
        Some(p) => Ok(p),
        None => bail!("{{{name}}} isn't available here (template {template:?})"),
    };
    let value = match name {
        "book" => vars.book.to_string(),
        "locator" => vars.locator.to_string(),
        "page" => page()?.to_string(),
        "page0" => (page()? - 1).to_string(),
        _ => bail!("unknown placeholder {{{name}}} in template {template:?}"),
    };
    Ok(format!("{value:0>width$}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(page: Option<u32>) -> Vars<'static> {
        Vars {
            book: "42",
            locator: "/srv/b/42.bin",
            page,
        }
    }

    #[test]
    fn expands_placeholders() {
        let s = expand("{locator}/{page:04}-{page0}-{book}.png", &vars(Some(7))).unwrap();
        assert_eq!(s, "/srv/b/42.bin/0007-6-42.png");
    }

    #[test]
    fn escapes_braces() {
        assert_eq!(expand("{{x}} {book}", &vars(None)).unwrap(), "{x} 42");
    }

    #[test]
    fn rejects_unknown_and_missing() {
        assert!(expand("{nope}", &vars(Some(1))).is_err());
        assert!(expand("{page}", &vars(None)).is_err());
        assert!(expand("{page", &vars(Some(1))).is_err());
        assert!(expand("{page:4}", &vars(Some(1))).is_err());
    }
}
