//! The builtin tool declarations the engine offers.
//!
//! Port of the declaration half of the Go `tools` package. The declarations are
//! what the model and the OpenCode free-tier gate see, so they are ported first
//! and kept verbatim; execution (`shell`, `set_directory`, `read`, `edit`,
//! `write`, `current_time`, `fetch_url`) is the next milestone in
//! `rust/MILESTONES.md`.
//!
//! `builtin()` is the hook the opencode adapter injects through
//! `Adapter::with_builtin`, which keeps the layering acyclic: tools depends on
//! provider, never the reverse.

use std::collections::HashMap;

use aixodia_provider::ToolDef;
use serde_json::{json, Map, Value};

fn params(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn shell_def() -> ToolDef {
    ToolDef {
        name: "shell".to_string(),
        description: "Executes a given shell command in a persistent shell session with optional timeout, \
            ensuring proper handling and security measures.\n\nBe aware: OS: linux, Shell: sh\n\n\
            All commands run in the directory set with the `set_directory` tool for this chat, or in the home \
            directory when none is set. Use the `workdir` parameter if you need a single command to run in a \
            different directory. AVOID using `cd <directory> && <command>` patterns - use `set_directory` once \
            for the project you are working in, or `workdir` for a one-off.\n\nThe command argument is required. \
            Output combines stdout and stderr; a non-zero exit is reported as `exit code N` after the output. \
            If the output is very large it is truncated."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute."
                },
                "timeout": {
                    "type": "number",
                    "description": "Optional timeout in milliseconds. Defaults to 120000, capped at 300000."
                },
                "workdir": {
                    "type": "string",
                    "description": "The working directory to run the command in. Defaults to the current directory. Use this instead of 'cd' commands."
                }
            },
            "required": ["command"]
        })),
    }
}

fn set_directory_def() -> ToolDef {
    ToolDef {
        name: "set_directory".to_string(),
        description: "Sets the working directory the `shell` tool uses for this chat, so commands stop \
            needing a `cd` prefix. The path is resolved once — symlinks and `..` are followed — and must \
            exist. It applies to every later `shell` call in this conversation, unless that call passes its \
            own `workdir`. Pass an empty path to clear the setting and fall back to the home directory."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the directory the shell should run in. A relative path is resolved against the current shell directory. An empty string clears the setting."
                }
            },
            "required": ["path"]
        })),
    }
}

fn read_def() -> ToolDef {
    ToolDef {
        name: "read".to_string(),
        description: "Read a file or directory from the local filesystem. If the path does not exist, an error is returned.\n\n\
            Usage:\n\
            - The filePath parameter should be an absolute path.\n\
            - By default, this tool returns up to 2000 lines from the start of the file.\n\
            - The offset parameter is the line number to start from (1-indexed).\n\
            - To read later sections, call this tool again with a larger offset.\n\
            - Contents are returned with each line prefixed by its line number as `<line>: <content>`.\n\
            - For directories, entries are returned one per line (without line numbers) with a trailing `/` for subdirectories.\n\
            - Any line longer than 2000 characters is truncated.\n\
            - Pictures cannot be read as text. Pass base64: true to have one shown to you as an image instead; it is shrunk first, and you receive the picture itself, not a base64 string."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "filePath": {
                    "type": "string",
                    "description": "The absolute path to the file or directory to read."
                },
                "offset": {
                    "type": "number",
                    "description": "The line number to start reading from (1-indexed). Defaults to 1."
                },
                "limit": {
                    "type": "number",
                    "description": "The maximum number of lines to read (defaults to 2000)."
                },
                "base64": {
                    "type": "boolean",
                    "description": "Return the picture as an image you can see, instead of as text. Pictures are shrunk to a size a model can use first."
                }
            },
            "required": ["filePath"]
        })),
    }
}

fn edit_def() -> ToolDef {
    ToolDef {
        name: "edit".to_string(),
        description: "Edit a file by replacing exact text. oldString must occur exactly once in the file. Returns a short confirmation.".to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "filePath": {
                    "type": "string",
                    "description": "Absolute path to the file to edit."
                },
                "oldString": {
                    "type": "string",
                    "description": "Exact text to find and replace."
                },
                "newString": {
                    "type": "string",
                    "description": "Text to replace oldString with."
                }
            },
            "required": ["filePath", "oldString", "newString"]
        })),
    }
}

fn write_def() -> ToolDef {
    ToolDef {
        name: "write".to_string(),
        description: "Write text to a file, creating it and any missing parent directories. \
            An existing file is replaced completely. Returns how many bytes were written."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "filePath": {
                    "type": "string",
                    "description": "Absolute path to the file to write."
                },
                "content": {
                    "type": "string",
                    "description": "The complete new contents of the file."
                }
            },
            "required": ["filePath", "content"]
        })),
    }
}

fn current_time_def() -> ToolDef {
    ToolDef {
        name: "current_time".to_string(),
        description: "Returns the current date and time, optionally in a specific IANA time zone."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "IANA time zone name, for example Asia/Bangkok. Defaults to UTC."
                }
            }
        })),
    }
}

fn fetch_url_def() -> ToolDef {
    ToolDef {
        name: "fetch_url".to_string(),
        description: "Fetches an http or https URL with GET and returns the response body as text."
            .to_string(),
        parameters: params(json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http or https URL to fetch."
                }
            },
            "required": ["url"]
        })),
    }
}

/// Every builtin declaration, keyed by tool name, in the shape the adapters and
/// the gateway consume.
///
/// The command tool is `shell`, not `bash`: that is the name the real OpenCode
/// client (v2) sends and the one the free-tier gate recognises.
pub fn builtin() -> HashMap<String, ToolDef> {
    [
        ("shell", shell_def()),
        ("set_directory", set_directory_def()),
        ("read", read_def()),
        ("edit", edit_def()),
        ("write", write_def()),
        ("current_time", current_time_def()),
        ("fetch_url", fetch_url_def()),
    ]
    .into_iter()
    .map(|(name, def)| (name.to_string(), def))
    .collect()
}

/// The declaration list in a stable order, for a request body.
pub fn builtin_list() -> Vec<ToolDef> {
    let defs = builtin();
    [
        "shell",
        "set_directory",
        "read",
        "edit",
        "write",
        "current_time",
        "fetch_url",
    ]
    .into_iter()
    .filter_map(|name| defs.get(name).cloned())
    .collect()
}

/// A handle the opencode adapter can inject.
pub fn builtin_fn() -> aixodia_provider::opencode::BuiltinFn {
    std::sync::Arc::new(builtin)
}

pub mod dir;
pub mod obsidian;
pub mod run;

pub use aixodia_goals as goals;
pub use aixodia_goals::{Item as Objective, Kind as ObjectiveKind, Store as ObjectiveStore};
pub use run::{run_tool, tools_with, with_reason, Tool, ToolContext};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_cover_the_contract_tools() {
        let defs = builtin();
        // The free-tier gate requires both, with real parameter schemas.
        for name in ["shell", "read"] {
            let def = defs.get(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(def.name, name);
            assert_eq!(
                def.parameters.get("type").and_then(Value::as_str),
                Some("object")
            );
            assert!(!def
                .parameters
                .get("properties")
                .expect("properties")
                .is_null());
            assert!(!def.description.is_empty());
        }
    }

    #[test]
    fn list_is_stable_and_complete() {
        let names: Vec<String> = builtin_list().into_iter().map(|def| def.name).collect();
        assert_eq!(
            names,
            [
                "shell",
                "set_directory",
                "read",
                "edit",
                "write",
                "current_time",
                "fetch_url"
            ]
        );
    }
}
