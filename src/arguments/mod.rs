/// Contains all functionality pertaining to parsing, tokenizing, and generating input arguments.
pub mod errors;
mod jobs;
mod man;

use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::exit;
use permutate::Permutator;
use num_cpus;

use super::disk_buffer::{self, DiskBufferTrait};
use super::filepaths;
use super::input_iterator::InputIterator;
use super::tokenizer::{Token, tokenize};
use self::errors::ParseErr;

// Re-export key items from internal modules.
pub use self::errors::{FileErr, InputIteratorErr};


#[derive(PartialEq)]
enum Mode { Arguments, Command, Inputs, Files }

#[derive(Clone)]
pub struct Flags {
    pub inputs_are_commands: bool,
    pub pipe:                bool,
    pub uses_shell:          bool,
    pub quiet:               bool,
    pub verbose:             bool
}

impl Flags {
    fn new() -> Flags {
        Flags {
            inputs_are_commands: false,
            uses_shell: true,
            quiet: false,
            verbose: false,
            pipe: false,
        }
    }
}

/// `Args` is a collection of critical options and arguments that were collected at
/// startup of the application.
pub struct Args {
    pub flags:        Flags,
    pub ncores:       usize,
    pub arguments:    Vec<Token>,
    pub piped_values: Option<Vec<String>>,
    pub ninputs:      usize,
}

impl Args {
    pub fn new() -> Args {
        Args {
            ncores:       num_cpus::get(),
            flags:        Flags::new(),
            arguments:    Vec::new(),
            piped_values: None,
            ninputs:      0,
        }
    }

    pub fn parse(&mut self) -> Result<InputIterator, ParseErr> {
        let unprocessed_path = match filepaths::unprocessed() {
            Some(path) => path,
            None => return Err(ParseErr::File(FileErr::Path))
        };

        let mut quote = false;
        let mut shellquote = false;

        // Create a write buffer that automatically writes data to the disk when the buffer is full.
        let mut disk_buffer = disk_buffer::DiskBuffer::new(&unprocessed_path).write()
            .map_err(|why| ParseErr::File(FileErr::Open(unprocessed_path.clone(), why)))?;

        // Temporary stores for input arguments.
        let mut raw_args                    = env::args().skip(1).peekable();
        let mut comm                        = String::with_capacity(128);
        let mut lists: Vec<Vec<String>>     = Vec::new();
        let mut current_inputs: Vec<String> = Vec::new();

        // The purpose of this is to set the initial parsing mode.
        let mut mode = match raw_args.peek().ok_or(ParseErr::NoArguments)?.as_ref() {
            ":::"  => Mode::Inputs,
            "::::" => Mode::Files,
            _      => Mode::Arguments
        };

        // If there are no arguments to be parsed, then the inputs are commands.
        self.flags.inputs_are_commands = mode == Mode::Inputs || mode == Mode::Files;

        // Parse each and every input argument supplied to the program.
        while let Some(argument) = raw_args.next() {
            let argument = argument.as_str();
            match mode {
                Mode::Arguments => {
                    let mut char_iter = argument.chars().peekable();

                    // If the first character is a '-' then it will be processed as an argument.
                    // We can guarantee that there will always be at least one character.
                    if char_iter.next().unwrap() == '-' {
                        // If the second character exists, everything's OK.
                        if let Some(character) = char_iter.next() {
                            // This scope of code allows users to utilize the GNU style
                            // command line arguments, to allow for laziness.
                            if character == 'j' {
                                // The short-hand job argument needs to be handled specially.
                                self.ncores = if char_iter.peek().is_some() {
                                    // Each character that follows after `j` will be considered an
                                    // input value.
                                    jobs::parse(&argument[2..])?
                                } else {
                                    // If there was no character after `j`, the following argument
                                    // must be the job value.
                                    let val = &raw_args.next().ok_or(ParseErr::JobsNoValue)?;
                                    jobs::parse(val)?
                                }
                            } else if character != '-' {
                                // NOTE: Short mode versions of arguments
                                for character in argument[1..].chars() {
                                    match character {
                                        'h' => {
                                            println!("{}", man::MAN_PAGE);
                                            exit(0);
                                        },
                                        'n' => self.flags.uses_shell = false,
                                        'p' => self.flags.pipe = true,
                                        'q' => quote = true,
                                        's' => self.flags.quiet = true,
                                        'v' => self.flags.verbose = true,
                                        _ => {
                                            return Err(ParseErr::InvalidArgument(argument.to_owned()))
                                        }
                                    }
                                }
                            } else {
                                // NOTE: Long mode versions of arguments
                                match &argument[2..] {
                                    "help" => {
                                        println!("{}", man::MAN_PAGE);
                                        exit(0);
                                    },
                                    "jobs" => {
                                        let val = &raw_args.next().ok_or(ParseErr::JobsNoValue)?;
                                        self.ncores = jobs::parse(val)?
                                    },
                                    "no-shell" => self.flags.uses_shell = false,
                                    "num-cpu-cores" => {
                                        println!("{}", num_cpus::get());
                                        exit(0);
                                    },
                                    "pipe" => self.flags.pipe = true,
                                    "quiet" | "silent" => self.flags.quiet = true,
                                    "quote" => quote = true,
                                    "shellquote" => shellquote = true,
                                    "verbose" => self.flags.verbose = true,
                                    "version" => {
                                        println!("parallel 0.6.2\n\nCrate Dependencies:");
                                        println!("    libc      0.2.15");
                                        println!("    num_cpus  1.0.0");
                                        println!("    permutate 0.1.3");
                                        exit(0);
                                    }
                                    _ => {
                                        return Err(ParseErr::InvalidArgument(argument.to_owned()));
                                    }
                                }
                            }
                        } else {
                            // `-` will never be a valid argument
                            return Err(ParseErr::InvalidArgument("-".to_owned()));
                        }
                    } else {
                        match argument {
                            ":::" => {
                                mode = Mode::Inputs;
                                self.flags.inputs_are_commands = true;
                            },
                            "::::" => {
                                mode = Mode::Files;
                                self.flags.inputs_are_commands = true;
                            }
                            _ => {
                                // The command has been supplied, and argument parsing is over.
                                comm.push_str(argument);
                                mode = Mode::Command;
                            }
                        }
                    }
                },
                Mode::Command => match argument {
                    // Arguments after `:::` are input values.
                    ":::" | ":::+" => mode = Mode::Inputs,
                    // Arguments after `::::` are files with inputs.
                    "::::" | "::::+" => mode = Mode::Files,
                    // All other arguments are command arguments.
                    _ => {
                        comm.push(' ');
                        comm.push_str(argument);
                    }
                },
                _ => match argument {
                    // `:::` denotes that the next set of inputs will be added to a new list.
                    ":::"  => {
                        mode = Mode::Inputs;
                        if !current_inputs.is_empty() {
                            lists.push(current_inputs.clone());
                            current_inputs.clear();
                        }
                    },
                    // `:::+` denotes that the next set of inputs will be added to the current list.
                    ":::+" => mode = Mode::Inputs,
                    // `::::` denotes that the next set of inputs will be added to a new list.
                    "::::"  => {
                        mode = Mode::Files;
                        if !current_inputs.is_empty() {
                            lists.push(current_inputs.clone());
                            current_inputs.clear();
                        }
                    },
                    // `:::+` denotes that the next set of inputs will be added to the current list.
                    "::::+" => mode = Mode::Files,
                    // All other arguments will be added to the current list.
                    _ => match mode {
                        Mode::Inputs => current_inputs.push(argument.to_owned()),
                        Mode::Files => file_parse(&mut current_inputs, argument)?,
                        _ => unreachable!()
                    }
                }
            }
        }

        if !current_inputs.is_empty() {
            lists.push(current_inputs.clone());
        }

        let mut number_of_arguments = 0;

        if lists.len() > 1 {
            // Convert the Vec<Vec<String>> into a Vec<Vec<&str>>
            let tmp: Vec<Vec<&str>> = lists.iter()
                .map(|list| list.iter().map(AsRef::as_ref).collect::<Vec<&str>>())
                .collect();

            // Convert the Vec<Vec<&str>> into a Vec<&[&str]>
            let list_array: Vec<&[&str]> = tmp.iter().map(AsRef::as_ref).collect();

            // Create a `Permutator` with the &[&[&str]] as the input.
            let permutator = Permutator::new(&list_array[..]);

            for permutation in permutator {
                let mut iter = permutation.iter();
                disk_buffer.write(iter.next().unwrap().as_bytes())
                    .map_err(|why| ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                for element in iter {
                    disk_buffer.write_byte(b' ')
                        .map_err(|why| ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                    disk_buffer.write(element.as_bytes())
                        .map_err(|why| ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                }
                disk_buffer.write_byte(b'\n')
                    .map_err(|why| ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                number_of_arguments += 1;
            }
        } else {
            for input in current_inputs {
                disk_buffer.write(input.as_bytes()).map_err(|why|
                    ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                disk_buffer.write_byte(b'\n').map_err(|why|
                    ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                number_of_arguments += 1;
            }
        }

        // If no inputs are provided, read from stdin instead.
        if disk_buffer.is_empty() {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                if let Ok(line) = line {
                    disk_buffer.write(line.as_bytes()).map_err(|why|
                        ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                    disk_buffer.write_byte(b'\n').map_err(|why|
                        ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;
                    number_of_arguments += 1;
                }
            }
        }

        // Flush the contents of the buffer to the disk before tokenizing the command argument.
        disk_buffer.flush().map_err(|why|
            ParseErr::File(FileErr::Write(disk_buffer.path.clone(), why)))?;

        // Expand the command if quoting is enabled
        if shellquote { comm = shellquote_command(comm); } else if quote { comm = quote_command(comm); }

        // Attempt to tokenize the command argument into simple primitive placeholders.
        tokenize(&mut self.arguments, &comm, &unprocessed_path, number_of_arguments)
            .map_err(ParseErr::Token)?;

        // Return an `InputIterator` of the arguments contained within the unprocessed file.
        let path = filepaths::unprocessed().ok_or(ParseErr::File(FileErr::Path))?;
        Ok(InputIterator::new(&path, number_of_arguments).map_err(ParseErr::File)?)
    }
}

fn quote_command(command: String) -> String {
    let mut output = String::with_capacity(command.len() << 1);
    for character in command.chars() {
        match character {
            '\\' => {
                output.push('\\');
                output.push(character);
            },
            _ => output.push(character)
        }
    }
    output
}

fn shellquote_command(command: String) -> String {
    let mut command_found = false;
    let mut output = String::with_capacity(command.len() << 1);
    for character in command.chars() {
        match character {
            ' ' if !command_found => {
                command_found = true;
            }
            '$' | ' ' | '\\' | '>' | '<' | '^' | '&' | '#' | '!' | '*' | '\'' | '\"' | '`' | '~' | '{' | '}' | '[' |
            ']' | '(' | ')' | ';' | '|' | '?' => output.push('\\'),
            _ => ()
        }
        output.push(character);
    }
    output
}

/// Attempts to open an input argument and adds each line to the `inputs` list.
fn file_parse<P: AsRef<Path>>(inputs: &mut Vec<String>, path: P) -> Result<(), ParseErr> {
    let path = path.as_ref();
    let file = fs::File::open(path).map_err(|err| ParseErr::File(FileErr::Open(path.to_owned(), err)))?;
    for line in BufReader::new(file).lines() {
        if let Ok(line) = line { inputs.push(line); }
    }
    Ok(())
}
