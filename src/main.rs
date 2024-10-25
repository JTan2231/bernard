use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

mod config;
mod logger;

use crate::logger::Logger;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum RequestMethod {
    Completion,
    Analysis,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Request {
    method: RequestMethod,
    body: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AnalysisRequest {
    user_query: String,
    body: String,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum DiffType {
    Addition,
    Deletion,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Diff {
    diff_type: DiffType,
    line: usize,
    delta: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileChange {
    pub filename: String,
    pub diffs: Vec<Diff>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Cursor {
    pub line: u32,
    pub column: u32,
    pub flat: u32,
    pub filename: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SuggestionRequest {
    pub changes: Vec<FileChange>,
    pub cursor: Cursor,
    pub cursor_context: String,
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
//     },
//     "cursor_context": String
// }
// where each Diff is the struct defined above

const COMPLETION_PROMPT: &str = r#"
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
take special account of where the cursor is placed (indicated by something resembling `-----^`) so as to not repeat what the user has already typed
"#;

const LABEL_PROMPT: &str = r#"
how would you describe this code?
write a short code comment to answer the above question.
your response _must_ be _only_ a code-compliant comment--this cannot introduce compiler/interpreter errors.
"#;

const MATCH_PROMPT: &str = r#"
you will be given two inputs:
- a single <descriptor> tag
- a series <code> tags

your task is a logical OR of the following two conditions:
- determine whether the given code under the <code> tags is aptly described by the description under the <descriptor> tag
- determine whether the techniques used to accomplish the objectives implicit in the <code> tags are suitable to the <descriptor> tag (note that this is programming language agnostic)

your response must be a JSON array of strings of either `yes` or `no`--_nothing else_
"#;

const ANALYSIS_PROMPT: &str = r#"
how could this code under the <input> tag be refactored to be more consistent with what's being done under the <reference> tags, (optionally, if it's here) according to the user's wishes under the <query> tag?
alternatively (or in addition to the above), how can the code in the <input> tag be improved with the <reference> tags being optionally referenced?

note that there may or may not be reference tags

the refactored code _must_ retain its original purpose and function

your response can be any ONE of the following three options:
- a refactoring of what's given in the <input> tag
- a code addition to what's given in the <input> tag
- <NOP>, if you believe there is nothing to be done here

additionally, your response must include a _code_ comment explaining your choice

_your response must only be in code/code comments--no markdown or anything else (unless it's <NOP>)_
"#;

fn prompt_tllm(system_prompt: &str, input: &str) -> Result<String, std::io::Error> {
    match std::process::Command::new("tllm")
        .arg("-s")
        .arg(system_prompt)
        .arg("-n")
        .arg("-i")
        .arg(input)
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

            Ok(out)
        }
        Err(e) => {
            error!("Error reading tllm output: {}", e);
            return Err(e);
        }
    }
}

macro_rules! prompt {
    ($p:expr, $input:expr) => {
        match prompt_tllm($p, $input) {
            Ok(response) => response,
            Err(e) => {
                error!("error prompting TLLM: {}", e);
                return Err(e);
            }
        }
    };
}

fn completion(
    stream: &mut std::net::TcpStream,
    request: &SuggestionRequest,
) -> Result<(), std::io::Error> {
    let mut diff_string = String::new();

    for file_change in request.changes.iter() {
        diff_string += &format!("@@@ {}\n", file_change.filename);
        for i in 0..file_change.diffs.len() {
            let diff = &file_change.diffs[i];
            match diff.diff_type {
                DiffType::Addition => {
                    diff_string += &format!("{} + {}\n", diff.line, diff.delta);
                }
                DiffType::Deletion => {
                    diff_string += &format!("{} - {}\n", diff.line, diff.delta);
                }
            }

            if i > 0 && diff.line - file_change.diffs[i - 1].line > 1 {
                diff_string += "...\n";
            }
        }
    }

    let cursor_indicator = "-"
        .repeat(request.cursor.line.to_string().len() + 1 + request.cursor.column as usize)
        .to_string()
        + "^";
    let cursor_context = format!("{}\n{}", request.cursor_context, cursor_indicator);

    let user_prompt = format!(
        "# Diff\n{}\n#########\n# LastInput\n{}",
        diff_string, cursor_context
    );

    let completion = prompt!(COMPLETION_PROMPT, &user_prompt);

    match stream.write_all(completion.as_bytes()) {
        Ok(_) => {
            info!("Completion size {}", completion.len());
        }
        Err(e) => {
            error!("Error writing to stream: {}", e);
            return Err(e);
        }
    }

    Ok(())
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeweyRequest {
    pub k: usize,
    pub query: String,
    pub filters: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeweyResponseItem {
    pub filepath: String,
    pub subset: (u64, u64),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct DeweyResponse {
    pub results: Vec<DeweyResponseItem>,
}

// working with an arbitrary cap of 8192 characters
// loosely based on the 8192 token limit
// user input is limited to 2048 characters
// references are limited to the rest
//
// naive, yes--we need a tokenizer here
fn analysis(
    stream: &mut std::net::TcpStream,
    request: &AnalysisRequest,
) -> Result<(), std::io::Error> {
    let user_query = request.user_query.clone();
    let user_context = &request.body[0..std::cmp::min(2048, request.body.len())].to_string();
    let start = request.byte_start;
    let end = request.byte_end;

    let label = prompt!(LABEL_PROMPT, user_context);

    let mut dewey_connection = match std::net::TcpStream::connect("127.0.0.1:5051") {
        Ok(dc) => dc,
        Err(e) => {
            error!("error establishing dewey connection: {}", e);
            return Err(e);
        }
    };

    let request = serde_json::to_string(&DeweyRequest {
        k: 10,
        query: user_context.clone(),
        filters: Vec::new(),
    })?
    .into_bytes();

    let mut payload = Vec::new();
    payload.extend_from_slice(&(request.len() as u32).to_be_bytes());
    payload.extend_from_slice(&request);

    match dewey_connection.write(&payload) {
        Ok(_) => {}
        Err(e) => {
            error!("error writing DeweyRequest to TCP stream: {}", e);
            return Err(e);
        }
    };

    let mut length_bytes = [0u8; 4];
    dewey_connection.read_exact(&mut length_bytes)?;
    let length = u32::from_be_bytes(length_bytes) as usize;

    let mut buffer = vec![0u8; length];
    dewey_connection.read_exact(&mut buffer)?;

    let buffer = String::from_utf8_lossy(&buffer);

    let dewey_matches: Vec<DeweyResponseItem> = match serde_json::from_str::<DeweyResponse>(&buffer)
    {
        Ok(dr) => dr.results,
        Err(e) => {
            error!("error parsing dewey response: {}", e);
            return Err(e.into());
        }
    };

    if dewey_matches.len() == 0 {
        match stream.write_all("<NOP>".as_bytes()) {
            Ok(_) => {
                info!("no Dewey results, <NOP> sent");
            }
            Err(e) => {
                error!("Error writing to stream: {}", e);
                return Err(e);
            }
        };

        return Ok(());
    }

    info!("looking through references...");

    let mut match_input = format!("<descriptor>{}</descriptor>", label);
    let mut total_len = 0;
    let mut references = Vec::new();
    for dewey_result in dewey_matches {
        if (start >= dewey_result.subset.0 as usize && start <= dewey_result.subset.1 as usize)
            || (end >= dewey_result.subset.0 as usize && end <= dewey_result.subset.1 as usize)
        {
            continue;
        }

        let file_contents = match std::fs::read_to_string(dewey_result.filepath.clone()) {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "error reading file contents of {}: {}",
                    dewey_result.filepath, e
                );

                continue;
            }
        };

        let subset = dewey_result.subset;
        let selected_result = &file_contents[subset.0 as usize..subset.1 as usize];

        total_len += selected_result.len();
        references.push(selected_result.to_string());
    }

    let difference = std::cmp::max(0, total_len - 6144);

    for reference in references.iter() {
        if reference.len() <= difference {
            continue;
        }

        match_input.push_str(&format!(
            "\n<code>{}</code>",
            &reference[0..reference.len() - difference]
        ));
    }

    let matches = prompt!(MATCH_PROMPT, &match_input);
    let matches = matches.trim();
    let matches = serde_json::from_str::<Vec<String>>(matches)?;

    let mut valid_references = Vec::new();
    for (i, m) in matches.iter().enumerate() {
        if m == "yes" {
            valid_references.push(references[i].clone());
        }
    }

    info!("using matches...");

    if valid_references.len() > 0 {
        let mut analysis_input = if user_query.len() > 0 {
            format!("<query>{}</query>\n", user_query)
        } else {
            String::new()
        };

        analysis_input.push_str(&format!("<input>{}</input>", user_context));

        for reference in valid_references {
            analysis_input.push_str(&format!("\n<reference>{}</reference>", reference));
        }

        let suggestion = prompt!(ANALYSIS_PROMPT, &analysis_input);
        match stream.write_all(suggestion.as_bytes()) {
            Ok(_) => {
                info!("Analysis size {}", suggestion.len());
            }
            Err(e) => {
                error!("Error writing to stream: {}", e);
                return Err(e);
            }
        };
    } else {
        match stream.write_all("<NOP>".as_bytes()) {
            Ok(_) => {
                info!("GPT decided <NOP> sent");
            }
            Err(e) => {
                error!("Error writing to stream: {}", e);
                return Err(e);
            }
        };
    }

    Ok(())
}

fn main() -> std::io::Result<()> {
    config::setup()?;

    let listener = TcpListener::bind("127.0.0.1:5050").unwrap();
    info!("Server listening on port 5050");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| {
                    let mut stream = stream;
                    let mut size_buffer = [0u8; 4];
                    stream.read_exact(&mut size_buffer).unwrap();
                    let message_size = u32::from_be_bytes(size_buffer) as usize;

                    let mut buffer = vec![0u8; message_size];
                    stream.read_exact(&mut buffer).unwrap();

                    let buffer = String::from_utf8_lossy(&buffer);
                    let request: Request = serde_json::from_str(&buffer).unwrap();

                    match request.method {
                        RequestMethod::Completion => {
                            let changes: SuggestionRequest =
                                serde_json::from_str(&request.body).unwrap();
                            match completion(&mut stream, &changes) {
                                Ok(_) => {}
                                Err(e) => {
                                    error!("error running completion: {}", e);
                                    panic!();
                                }
                            }
                        }
                        RequestMethod::Analysis => {
                            let request: AnalysisRequest =
                                serde_json::from_str(&request.body).unwrap();
                            match analysis(&mut stream, &request) {
                                Ok(_) => {}
                                Err(e) => {
                                    error!("error running analysis: {}", e);
                                    panic!();
                                }
                            }
                        }
                    };
                });
            }
            Err(e) => {
                info!("Error: {}", e);
            }
        }
    }

    Ok(())
}
