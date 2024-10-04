use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

mod config;
mod logger;

use crate::logger::Logger;

#[derive(Debug)]
enum DiffType {
    Addition,
    Deletion,
}

#[derive(Debug)]
struct Diff {
    diff_type: DiffType,
    delta: String,
}

// expected input JSON format:
// {
//     "changes": [
//         {
//           "filename": String,
//           "diffs": [Diff, ...]
//         },
//         ...
//     ],
//     "cursor": {
//         "line": u32,
//         "column": u32,
//         "flat": u32,
//         "filename": String
//     }
// }
// where each Diff is the struct defined above

const SYSTEM_PROMPT: &str = r#"
you will be prompted with a 2 piece message:
- context
  - a series of diffs
    - note that this may or may not represent the actual changes, or just what they currently are
  - a collection of related function definitions or signatures, as size permits
- the current position and surrounding context of a user's cursor

from these bits of information, you need to provide a completion
that represents the most likely thing the user will type

do not restate anything you are given
do not explain anything
_only_ provide the completion to append to the end of what you are given
be mindful of trailing whitespace and the like
suggest with _discretion_--don't get in the way, but make suggestions that _clearly go along with the user's intent_
be judicious in assuming what the user is attempting
"#;

fn main() -> std::io::Result<()> {
    config::setup()?;

    let listener = TcpListener::bind("127.0.0.1:5050").unwrap();
    info!("Server listening on port 5050");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| {
                    let mut stream = stream;
                    let mut buffer = [0; 16384];

                    let size = match stream.read(&mut buffer) {
                        Ok(size) => size,
                        Err(e) => {
                            error!("Error reading stream buffer: {}", e);
                            return;
                        }
                    };

                    let request = String::from_utf8_lossy(&buffer[..size]);
                    let request: serde_json::Value = match serde_json::from_str(&request) {
                        Ok(request) => request,
                        Err(e) => {
                            error!("Error: {}", e);
                            return;
                        }
                    };

                    let mut diff_map = std::collections::HashMap::new();
                    for obj in request["changes"].as_array().unwrap() {
                        let filename = obj["filename"].as_str().unwrap();
                        let obj_diffs = obj["diffs"].as_array().unwrap();

                        let diff_vec = diff_map.entry(filename).or_insert(Vec::new());
                        for diff in obj_diffs {
                            let diff_type = match diff["diff_type"].as_str().unwrap() {
                                "addition" => DiffType::Addition,
                                "deletion" => DiffType::Deletion,
                                _ => {
                                    error!("Invalid diff type");
                                    return;
                                }
                            };

                            let delta = diff["delta"].as_str().unwrap();

                            diff_vec.push(Diff {
                                diff_type,
                                delta: delta.to_string(),
                            });
                        }
                    }

                    let mut diff_string = String::new();

                    for (filename, diffs) in diff_map.iter() {
                        diff_string += &format!("@@@ {}\n", filename);
                        for i in 0..diffs.len() {
                            let diff = &diffs[i];
                            let lines = diff.delta.split('\n').collect::<Vec<_>>();
                            for line in lines {
                                match diff.diff_type {
                                    DiffType::Addition => {
                                        diff_string += &format!("+ {}\n", line);
                                    }
                                    DiffType::Deletion => {
                                        diff_string += &format!("- {}\n", line);
                                    }
                                }
                            }
                        }
                    }

                    let cursor_column = request["cursor"]["column"].as_u64().unwrap() as usize;
                    let cursor_indicator = "-".repeat(cursor_column - 1).to_string() + "^";
                    let cursor_context = format!(
                        "{}\n{}",
                        request["cursor_context"].as_str().unwrap(),
                        cursor_indicator
                    );

                    let prompt = format!(
                        "# Diff\n{}\n#########\n# LastInput\n{}",
                        diff_string, cursor_context
                    );

                    info!("Prompt:\n{}", prompt);

                    let completion = match std::process::Command::new("tllm")
                        .arg("-s")
                        .arg(SYSTEM_PROMPT)
                        .arg("-n")
                        .arg("-i")
                        .arg(prompt)
                        .output()
                    {
                        Ok(output) => {
                            let mut out = String::from_utf8_lossy(&output.stdout).to_string();

                            out = out.trim_end().to_string();
                            out = out
                                .replace("\n", "\\n")
                                .replace("\r", "\\r")
                                .replace("\t", "\\t");

                            out = out.trim().to_string();
                            out.push('\n');

                            out
                        }
                        Err(e) => {
                            error!("Error reading tllm output: {}", e);
                            return;
                        }
                    };

                    info!("Completion: {}", completion);

                    match stream.write_all(completion.as_bytes()) {
                        Ok(_) => {
                            info!("Response \"{}\" sent", completion);
                        }
                        Err(e) => {
                            error!("Error writing to stream: {}", e);
                        }
                    }
                });
            }
            Err(e) => {
                info!("Error: {}", e);
            }
        }
    }

    Ok(())
}
