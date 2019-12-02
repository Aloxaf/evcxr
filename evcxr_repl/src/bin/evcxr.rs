// Copyright 2018 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use dirs;
use evcxr;

use colored::*;
use evcxr::{CommandContext, CompilationError, Error};
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::{error::ReadlineError, Context, Editor, Helper};
use std::borrow::Cow;
use std::fs;
use std::io;
use std::sync::mpsc;
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;

const PROMPT: &str = ">> ";

struct Repl {
    command_context: CommandContext,
}

fn send_output<T: io::Write + Send + 'static>(channel: mpsc::Receiver<String>, mut output: T) {
    std::thread::spawn(move || {
        while let Ok(line) = channel.recv() {
            if writeln!(output, "{}", line).is_err() {
                break;
            }
        }
    });
}

impl Repl {
    fn new() -> Result<Repl, Error> {
        let (command_context, outputs) = CommandContext::new()?;
        send_output(outputs.stdout, io::stdout());
        send_output(outputs.stderr, io::stderr());
        let mut repl = Repl { command_context };
        repl.execute(":load_config");
        Ok(repl)
    }

    fn execute(&mut self, to_run: &str) {
        match self.command_context.execute(to_run) {
            Ok(output) => {
                if let Some(text) = output.get("text/plain") {
                    println!("{}", text);
                }
                if let Some(duration) = output.timing {
                    println!("{}", format!("Took {}ms", duration.as_millis()).blue());

                    for phase in output.phases {
                        println!(
                            "{}",
                            format!("  {}: {}ms", phase.name, phase.duration.as_millis()).blue()
                        );
                    }
                }
            }
            Err(evcxr::Error::CompilationErrors(errors)) => {
                self.display_errors(errors);
            }
            Err(err) => eprintln!("{}", format!("{}", err).bright_red()),
        }
    }

    fn display_errors(&mut self, errors: Vec<CompilationError>) {
        for error in errors {
            if error.is_from_user_code() {
                for spanned_message in error.spanned_messages() {
                    if let Some(span) = &spanned_message.span {
                        for _ in 1..span.start_column + PROMPT.len() {
                            print!(" ");
                        }
                        let mut carrots = String::new();
                        for _ in span.start_column..span.end_column {
                            carrots.push('^');
                        }
                        print!("{}", carrots.bright_red());
                        println!(" {}", spanned_message.label.bright_blue());
                    } else {
                        // Our error originates from both user-code and generated
                        // code.
                        println!("{}", spanned_message.label.bright_blue());
                    }
                }
                println!("{}", error.message().bright_red());
                for help in error.help() {
                    println!("{} {}", "help:".bold(), help);
                }
                if let Some(extra_hint) = error.evcxr_extra_hint() {
                    println!("{}", extra_hint);
                }
            } else {
                println!(
                    "A compilation error was found in code we generated.\n\
                     Ideally this should't happen. Type :last_error_json to see details.\n{}",
                    error.rendered()
                );
            }
        }
    }
}

fn readline_direct(prompt: &str) -> rustyline::Result<String> {
    use std::io::Write;

    // Write prompt and flush it to stdout
    let mut stdout = io::stdout();
    stdout.write_all(prompt.as_bytes())?;
    stdout.flush()?;

    let mut line = String::new();
    if io::stdin().read_line(&mut line)? > 0 {
        Ok(line)
    } else {
        Err(rustyline::error::ReadlineError::Eof)
    }
}

struct RustHighlighter {
    ps: SyntaxSet,
    ts: ThemeSet,
}

impl RustHighlighter {
    fn new() -> RustHighlighter {
        let ps = SyntaxSet::load_defaults_nonewlines();
        let ts = ThemeSet::load_defaults();
        RustHighlighter { ps, ts }
    }

    fn highlight(&self, line: &str, _pos: usize) -> String {
        let syntax = self.ps.find_syntax_by_extension("rs").unwrap();
        let theme = &self.ts.themes["Solarized (dark)"];
        let mut h = HighlightLines::new(syntax, theme);
        as_24_bit_terminal_escaped(&h.highlight(line, &self.ps), false)
    }
}

struct RLHelper {
    highlighter: RustHighlighter,
    hinter: HistoryHinter,
}

impl RLHelper {
    fn new() -> RLHelper {
        RLHelper {
            highlighter: RustHighlighter::new(),
            hinter: HistoryHinter {},
        }
    }
}

impl Highlighter for RLHelper {
    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        Cow::Owned(self.highlighter.highlight(line, pos))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(hint.bright_black().to_string())
    }

    fn highlight_char(&self, _line: &str, _pos: usize) -> bool {
        true
    }
}

impl Hinter for RLHelper {
    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<String> {
        self.hinter.hint(line, pos, ctx)
    }
}

impl Completer for RLHelper {
    type Candidate = String;
}

impl Helper for RLHelper {}

fn main() {
    evcxr::runtime_hook();
    println!("Welcome to evcxr. For help, type :help");
    let mut repl = match Repl::new() {
        Ok(c) => c,
        Err(error) => {
            eprintln!("{}", error);
            return;
        }
    };

    let disable_readline = std::env::args().any(|x| x == "--disable-readline");

    let mut editor = Editor::new();
    editor.set_helper(Some(RLHelper::new()));
    let mut opt_history_file = None;
    let config_dir = dirs::config_dir().map(|h| h.join("evcxr"));
    if let Some(config_dir) = &config_dir {
        fs::create_dir_all(config_dir).ok();
        let history_file = config_dir.join("history.txt");
        editor.load_history(&history_file).ok();
        opt_history_file = Some(history_file);
    }
    loop {
        let prompt = format!("{}", PROMPT.yellow());
        let readline = if disable_readline {
            readline_direct(&prompt)
        } else {
            editor.readline(&prompt)
        };
        match readline {
            Ok(line) => {
                editor.add_history_entry(line.clone());
                repl.execute(&line);
            }
            Err(ReadlineError::Interrupted) => {
                println!("CTRL-C");
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("Error: {:?}", err);
                break;
            }
        }
    }
    if let Some(history_file) = &opt_history_file {
        editor.save_history(&history_file).ok();
    }
}
