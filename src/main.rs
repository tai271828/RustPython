#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate log;
extern crate rustyline;

use clap::{App, Arg};
use rustpython_compiler::{compile, error::CompileError, error::CompileErrorType};
use rustpython_parser::error::ParseErrorType;
use rustpython_vm::{
    frame::Scope,
    import,
    obj::objstr,
    print_exception,
    pyobject::{ItemProtocol, PyResult},
    util, VirtualMachine,
};

use rustyline::{error::ReadlineError, Editor};
use std::path::PathBuf;
use std::process;

fn main() {
    #[cfg(feature = "flame-it")]
    let main_guard = flame::start_guard("RustPython main");
    env_logger::init();
    let app = App::new("RustPython")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Rust implementation of the Python language")
        .arg(Arg::with_name("script").required(false).index(1))
        .arg(
            Arg::with_name("optimize")
                .short("O")
                .multiple(true)
                .help("Optimize. Set __debug__ to false. Remove debug statements."),
        )
        .arg(
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Give the verbosity"),
        )
        .arg(
            Arg::with_name("c")
                .short("c")
                .takes_value(true)
                .help("run the given string as a program"),
        )
        .arg(
            Arg::with_name("m")
                .short("m")
                .takes_value(true)
                .help("run library module as script"),
        )
        .arg(Arg::from_usage("[pyargs] 'args for python'").multiple(true));
    #[cfg(feature = "flame-it")]
    let app = app
        .arg(
            Arg::with_name("profile_output")
                .long("profile-output")
                .takes_value(true)
                .help("the file to output the profiling information to"),
        )
        .arg(
            Arg::with_name("profile_format")
                .long("profile-format")
                .takes_value(true)
                .help("the profile format to output the profiling information in"),
        );
    let matches = app.get_matches();

    let opt_level = matches.occurrences_of("optimize");
    let optimize = opt_level > 0;
    debug!("Optimize: {}", optimize);
    // Construct vm:
    let vm = VirtualMachine::new(optimize);

    let res = import::init_importlib(&vm, true);
    handle_exception(&vm, res);

    // Figure out if a -c option was given:
    let result = if let Some(command) = matches.value_of("c") {
        run_command(&vm, command.to_string())
    } else if let Some(module) = matches.value_of("m") {
        run_module(&vm, module)
    } else {
        // Figure out if a script was passed:
        match matches.value_of("script") {
            None => run_shell(&vm),
            Some(filename) => run_script(&vm, filename),
        }
    };

    // See if any exception leaked out:
    handle_exception(&vm, result);

    #[cfg(feature = "flame-it")]
    {
        main_guard.end();
        if let Err(e) = write_profile(matches) {
            error!("Error writing profile information: {}", e);
            process::exit(1);
        }
    }
}

#[cfg(feature = "flame-it")]
fn write_profile(matches: clap::ArgMatches) -> Result<(), Box<dyn std::error::Error>> {
    use std::fs::File;

    enum ProfileFormat {
        Html,
        Text,
        Speedscope,
    }

    let profile_output = matches.value_of_os("profile_output");

    let profile_format = match matches.value_of("profile_format") {
        Some("html") => ProfileFormat::Html,
        Some("text") => ProfileFormat::Text,
        None if profile_output == Some("-".as_ref()) => ProfileFormat::Text,
        Some("speedscope") | None => ProfileFormat::Speedscope,
        Some(other) => {
            error!("Unknown profile format {}", other);
            process::exit(1);
        }
    };

    let profile_output = profile_output.unwrap_or_else(|| match profile_format {
        ProfileFormat::Html => "flame-graph.html".as_ref(),
        ProfileFormat::Text => "flame.txt".as_ref(),
        ProfileFormat::Speedscope => "flamescope.json".as_ref(),
    });

    let profile_output: Box<dyn std::io::Write> = if profile_output == "-" {
        Box::new(std::io::stdout())
    } else {
        Box::new(File::create(profile_output)?)
    };

    match profile_format {
        ProfileFormat::Html => flame::dump_html(profile_output)?,
        ProfileFormat::Text => flame::dump_text_to_writer(profile_output)?,
        ProfileFormat::Speedscope => flamescope::dump(profile_output)?,
    }

    Ok(())
}

fn _run_string(vm: &VirtualMachine, source: &str, source_path: String) -> PyResult {
    let code_obj = vm
        .compile(source, &compile::Mode::Exec, source_path.clone())
        .map_err(|err| vm.new_syntax_error(&err))?;
    // trace!("Code object: {:?}", code_obj.borrow());
    let attrs = vm.ctx.new_dict();
    attrs.set_item("__file__", vm.new_str(source_path), vm)?;
    vm.run_code_obj(code_obj, Scope::with_builtins(None, attrs, vm))
}

fn handle_exception(vm: &VirtualMachine, result: PyResult) {
    if let Err(err) = result {
        print_exception(vm, &err);
        process::exit(1);
    }
}

fn run_command(vm: &VirtualMachine, mut source: String) -> PyResult {
    debug!("Running command {}", source);

    // This works around https://github.com/RustPython/RustPython/issues/17
    source.push('\n');
    _run_string(vm, &source, "<stdin>".to_string())
}

fn run_module(vm: &VirtualMachine, module: &str) -> PyResult {
    debug!("Running module {}", module);
    vm.import(module, &vm.ctx.new_tuple(vec![]), 0)
}

fn run_script(vm: &VirtualMachine, script_file: &str) -> PyResult {
    debug!("Running file {}", script_file);
    // Parse an ast from it:
    let file_path = PathBuf::from(script_file);
    let file_path = if file_path.is_file() {
        file_path
    } else if file_path.is_dir() {
        let main_file_path = file_path.join("__main__.py");
        if main_file_path.is_file() {
            main_file_path
        } else {
            error!(
                "can't find '__main__' module in '{}'",
                file_path.to_str().unwrap()
            );
            process::exit(1);
        }
    } else {
        error!(
            "can't open file '{}': No such file or directory",
            file_path.to_str().unwrap()
        );
        process::exit(1);
    };

    let dir = file_path.parent().unwrap().to_str().unwrap().to_string();
    let sys_path = vm.get_attribute(vm.sys_module.clone(), "path").unwrap();
    vm.call_method(&sys_path, "insert", vec![vm.new_int(0), vm.new_str(dir)])?;

    match util::read_file(&file_path) {
        Ok(source) => _run_string(vm, &source, file_path.to_str().unwrap().to_string()),
        Err(err) => {
            error!(
                "Failed reading file '{}': {:?}",
                file_path.to_str().unwrap(),
                err.kind()
            );
            process::exit(1);
        }
    }
}

#[test]
fn test_run_script() {
    let vm: VirtualMachine = Default::default();

    // test file run
    let r = run_script(&vm, "tests/snippets/dir_main/__main__.py");
    assert!(r.is_ok());

    // test module run
    let r = run_script(&vm, "tests/snippets/dir_main");
    assert!(r.is_ok());
}

fn shell_exec(vm: &VirtualMachine, source: &str, scope: Scope) -> Result<(), CompileError> {
    match vm.compile(source, &compile::Mode::Single, "<stdin>".to_string()) {
        Ok(code) => {
            match vm.run_code_obj(code, scope.clone()) {
                Ok(value) => {
                    // Save non-None values as "_"

                    use rustpython_vm::pyobject::{IdProtocol, IntoPyObject};

                    if !value.is(&vm.get_none()) {
                        let key = objstr::PyString::from("_").into_pyobject(vm);
                        scope.globals.set_item(key, value, vm).unwrap();
                    }
                }

                Err(err) => {
                    print_exception(vm, &err);
                }
            }

            Ok(())
        }
        // Don't inject syntax errors for line continuation
        Err(
            err @ CompileError {
                error: CompileErrorType::Parse(ParseErrorType::EOF),
                ..
            },
        ) => Err(err),
        Err(err) => {
            let exc = vm.new_syntax_error(&err);
            print_exception(vm, &exc);
            Err(err)
        }
    }
}

#[cfg(not(unix))]
fn get_history_path() -> PathBuf {
    PathBuf::from(".repl_history.txt")
}

#[cfg(unix)]
fn get_history_path() -> PathBuf {
    //work around for windows dependent builds. The xdg crate is unix specific
    //so access to the BaseDirectories struct breaks builds on python.
    extern crate xdg;

    let xdg_dirs = xdg::BaseDirectories::with_prefix("rustpython").unwrap();
    xdg_dirs.place_cache_file("repl_history.txt").unwrap()
}

fn get_prompt(vm: &VirtualMachine, prompt_name: &str) -> String {
    vm.get_attribute(vm.sys_module.clone(), prompt_name)
        .ok()
        .as_ref()
        .map(objstr::get_value)
        .unwrap_or_else(String::new)
}

fn run_shell(vm: &VirtualMachine) -> PyResult {
    println!(
        "Welcome to the magnificent Rust Python {} interpreter \u{1f631} \u{1f596}",
        crate_version!()
    );
    let vars = vm.new_scope_with_builtins();

    // Read a single line:
    let mut input = String::new();
    let mut repl = Editor::<()>::new();

    // Retrieve a `history_path_str` dependent on the OS
    let repl_history_path_str = &get_history_path();
    if repl.load_history(repl_history_path_str).is_err() {
        println!("No previous history.");
    }

    let mut continuing = false;

    loop {
        let prompt = if continuing {
            get_prompt(vm, "ps2")
        } else {
            get_prompt(vm, "ps1")
        };
        match repl.readline(&prompt) {
            Ok(line) => {
                debug!("You entered {:?}", line);
                input.push_str(&line);
                input.push('\n');
                repl.add_history_entry(line.trim_end());

                if continuing {
                    if line.is_empty() {
                        continuing = false;
                    } else {
                        continue;
                    }
                }

                match shell_exec(vm, &input, vars.clone()) {
                    Err(CompileError {
                        error: CompileErrorType::Parse(ParseErrorType::EOF),
                        ..
                    }) => {
                        continuing = true;
                        continue;
                    }
                    _ => {
                        input = String::new();
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                // TODO: Raise a real KeyboardInterrupt exception
                println!("^C");
                continuing = false;
                continue;
            }
            Err(ReadlineError::Eof) => {
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        };
    }
    repl.save_history(repl_history_path_str).unwrap();

    Ok(vm.get_none())
}
