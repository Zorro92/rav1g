// SPDX-License-Identifier: BSD-2-Clause
//
// This source code is subject to the terms of the BSD 2 Clause License and the Alliance for Open Media
// Patent License 1.0. If the BSD 2 Clause License was not distributed with this source code in the LICENSE
// file, you can obtain it at www.aomedia.org/license/software. If the Alliance for Open Media Patent
// License 1.0 was not distributed with this source code in the PATENTS file, you can obtain it at
// www.aomedia.org/license/patent.

//! The shader-pick experiment (the feature `shader-pick`; docs/test-player.md, "Shader pick"): the
//! library of that feature has two bytecodes of every shader, and a text says which one the pipeline
//! state of a shader is made of when a device is created:
//!
//! ```text
//! od=<list>,dxc=<list>
//! ```
//!
//! `dxc` is the default build's DXC bytecode (every shader, without a pick), `od` what
//! DXC compiled with `-Od` where supported. A list is
//! names of shaders separated by commas; `all` is every shader, and a name may have `*` for any
//! characters (`loopfilter_*`, `*_hbd`). The parts apply from left to right, so a later one overrides an
//! earlier one: `od=all,dxc=loopfilter_vert_hbd` uses `od` for everything but one shader.
//!
//! A library without the feature has one bytecode per shader and takes the empty pick only.

use std::sync::RwLock;

/// The bytecodes of a shader: `SHADERS` and `SHADERS_OD` of `crates/rav1g-sys/src/shaders.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Set {
    Dxc = 0,
    Od = 1,
}

const SETS: [(Set, &str); 2] = [(Set::Dxc, "dxc"), (Set::Od, "od")];

/// The name of a set in a pick and in the log.
pub(crate) fn label(set: Set) -> &'static str {
    SETS.iter().find(|(other, _)| *other == set).map_or("", |(_, label)| label)
}

/// The pick the next device is created with: the text as it was given.
static PICK: RwLock<String> = RwLock::new(String::new());

/// `name` matches `pattern`, in which `*` stands for any characters.
fn matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty(); // no '*': the whole name
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// The set of every shader of `names` by the pick `text`, or what is wrong with the text.
pub(crate) fn resolve(text: &str, names: &[&str]) -> Result<Vec<Set>, String> {
    let mut sets = vec![Set::Dxc; names.len()];
    let mut current = None;
    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
        let pattern = match item.split_once('=') {
            Some((set, pattern)) => {
                let set = set.trim();
                if set == "fxc" {
                    return Err("shader pick: the 'fxc' set has been removed. Use dxc or od.".to_string());
                }
                current = Some(
                    SETS.iter()
                        .find(|(_, name)| *name == set)
                        .map(|(set, _)| *set)
                        .ok_or_else(|| format!("shader pick: unknown set '{set}'. Use dxc or od."))?,
                );
                pattern.trim()
            }
            None => item,
        };
        let Some(set) = current else {
            return Err(format!("shader pick: '{item}' is in no set. Expected od=<list>,dxc=<list>."));
        };
        if pattern.is_empty() {
            return Err("shader pick: a set without a shader".to_string());
        }
        let mut found = false;
        for (name, slot) in names.iter().zip(&mut sets) {
            if pattern == "all" || matches(pattern, name) {
                *slot = set;
                found = true;
            }
        }
        if !found {
            return Err(format!("shader pick: no shader has the name '{pattern}'"));
        }
    }
    Ok(sets)
}

/// One log line: which shaders are not DXC's default bytecode, and of which set they are. The largest
/// set is not listed by name ("all others").
pub(crate) fn describe(text: &str, names: &[&str], sets: &[Set]) -> String {
    let count = |set: Set| sets.iter().filter(|s| **s == set).count();
    let largest = SETS.iter().map(|(set, _)| *set).max_by_key(|set| count(*set)).unwrap_or(Set::Dxc);
    let mut line =
        if text.trim().is_empty() { "SHADERPICK none:".to_string() } else { format!("SHADERPICK {}:", text.trim()) };
    for (set, label) in SETS {
        let n = count(set);
        line += &format!(" {label} {n}");
        if n == names.len() {
            line += " (all)";
        } else if n != 0 && set == largest {
            line += " (all others)";
        } else if n != 0 {
            let list: Vec<&str> = names.iter().zip(sets).filter(|(_, s)| **s == set).map(|(name, _)| *name).collect();
            line += &format!(" ({})", list.join(" "));
        }
        if set == Set::Dxc {
            line += ";";
        }
    }
    line
}

/// A log line cut into lines of at most `limit` bytes at its spaces (the log has lines of a fixed size);
/// the lines behind the first start with "SHADERPICK +".
pub(crate) fn wrap(line: &str, limit: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in line.split(' ') {
        let last = lines.last_mut().expect("one line at least");
        if !last.is_empty() && last.len() + 1 + word.len() > limit {
            lines.push(format!("SHADERPICK + {word}"));
        } else {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(word);
        }
    }
    lines
}

/// Sets the pick of the devices created from now on. An empty text is no pick: every shader `dxc`.
pub(crate) fn set(text: &str, names: &[&str]) -> Result<(), String> {
    let text = text.trim();
    if !text.is_empty() {
        resolve(text, names)?;
        if !cfg!(feature = "shader-pick") {
            return Err("shader pick: alternate bytecode requires the shader-pick build".to_string());
        }
    }
    *PICK.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = text.to_string();
    Ok(())
}

/// The pick as it was set.
pub(crate) fn get() -> String {
    PICK.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 5] = ["idct4x4", "loopfilter_vert", "loopfilter_vert_hbd", "loopfilter_hor_hbd", "inter_main"];

    #[test]
    fn patterns() {
        assert!(matches("loopfilter_*", "loopfilter_vert"));
        assert!(matches("*_hbd", "loopfilter_vert_hbd"));
        assert!(!matches("*_hbd", "loopfilter_vert"));
        assert!(matches("loop*vert*", "loopfilter_vert_hbd"));
        assert!(matches("*", "idct4x4"));
        assert!(!matches("loopfilter_vert", "loopfilter_vert_hbd"));
        assert!(matches("inter_main", "inter_main"));
        assert!(!matches("a*a", "a"));
    }

    #[test]
    fn picks() {
        use Set::*;
        assert_eq!(resolve("", &NAMES).unwrap(), [Dxc; 5]);
        assert_eq!(resolve("od=all", &NAMES).unwrap(), [Od; 5]);
        assert_eq!(resolve("od=loopfilter_vert_hbd,idct4x4,dxc=inter_main", &NAMES).unwrap(), [Od, Dxc, Od, Dxc, Dxc]);
        assert_eq!(resolve("od=all,dxc=loopfilter_vert_hbd", &NAMES).unwrap(), [Od, Od, Dxc, Od, Od]);
        assert_eq!(resolve("od=*_hbd, dxc=loopfilter_hor*", &NAMES).unwrap(), [Dxc, Dxc, Od, Dxc, Dxc]);
        assert!(resolve("od=nothing", &NAMES).is_err());
        assert!(resolve("idct4x4", &NAMES).is_err());
        assert!(resolve("o3=all", &NAMES).is_err());
        assert!(resolve("od=", &NAMES).is_err());
        assert_eq!(
            set("od=all,fxc=inter_main", &NAMES).unwrap_err(),
            "shader pick: the 'fxc' set has been removed. Use dxc or od."
        );
    }

    #[test]
    fn long_lines() {
        assert_eq!(wrap("SHADERPICK a: bb cc", 100), ["SHADERPICK a: bb cc"]);
        assert_eq!(
            wrap("SHADERPICK a: bbbb cccc dddd", 20),
            ["SHADERPICK a: bbbb", "SHADERPICK + cccc", "SHADERPICK + dddd"]
        );
    }

    #[test]
    fn lines() {
        let line = |text: &str| describe(text, &NAMES, &resolve(text, &NAMES).unwrap());
        assert_eq!(line(""), "SHADERPICK none: dxc 5 (all); od 0");
        assert_eq!(line("od=all"), "SHADERPICK od=all: dxc 0; od 5 (all)");
        assert_eq!(
            line("od=loopfilter_vert_hbd,inter_main"),
            "SHADERPICK od=loopfilter_vert_hbd,inter_main: dxc 3 (all others); od 2 (loopfilter_vert_hbd inter_main)"
        );
        assert_eq!(
            line("od=all,dxc=loopfilter_*_hbd"),
            "SHADERPICK od=all,dxc=loopfilter_*_hbd: dxc 2 (loopfilter_vert_hbd loopfilter_hor_hbd); od 3 (all others)"
        );
    }
}
