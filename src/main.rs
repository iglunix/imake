#![feature(if_let_guard)]
#![feature(array_from_fn)]

use std::{
    collections::HashMap,
    fs::File,
    io::{prelude::*, BufReader},
    iter::Peekable,
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
};

use glob;

mod expand;

// Global makefile state
#[derive(Default, Debug)]
struct State {
    debug: bool,
    fullname: String,
    basename: String,
    dirname: String,
    curdir: String,
    // vars: HashMap<String, Var>,
    always_make: bool,
    targets_to_make: Vec<String>,
    silent: bool,
    rules: Vec<Rule>,
    in_rule: bool,
    ignore_errors: bool,
    dryrun: bool,
    keep_going: bool,
    /// List of phony target names
    phony: Vec<String>,
    silent_targets: Vec<String>,
    processed: Vec<String>,
}

fn fatal_double_and_single(loc: &Location, target: &str) -> ! {
    println!("{}:{}: *** target file '{}' has both : and :: entries.  Stop", loc.file_name, loc.line, target);
    std::process::exit(2)
}

fn fatal_arg_count(loc: &Location, given: usize, func: &str) -> ! {
    println!(
        "{}:{}: *** insufficient number of arguments ({}) to function '{}'.  Stop.",
        loc.file_name, loc.line, given, func
    );
    std::process::exit(2)
}

fn fatal_unterm_var(loc: &Location) -> ! {
    println!(
        "{}:{}: *** unterminated variable reference.  Stop.",
        loc.file_name, loc.line
    );
    std::process::exit(2)
}

fn get_all_args(loc: &Location, func: &str, src: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut buf = String::new();
    let mut delim_stack = String::new();
    let mut src = src.chars();

    while match src.next() {
        Some(')') if delim_stack.chars().last().unwrap() == '(' => {
            delim_stack.pop();
            buf.push(')');
            true
        }
        Some('}') if delim_stack.chars().last().unwrap() == '{' => {
            delim_stack.pop();
            buf.push('}');
            true
        }
        Some('}') if delim_stack.chars().last().unwrap() == '(' => fatal_unterm_var(loc),
        Some(')') if delim_stack.chars().last().unwrap() == '{' => fatal_unterm_var(loc),
        Some('(') => {
            delim_stack.push('(');
            buf.push('(');
            true
        }
        Some('{') => {
            delim_stack.push('{');
            buf.push('{');
            true
        }
        Some(',') if delim_stack.is_empty() => {
            args.push(buf);
            buf = String::new();
            true
        }
        Some(a) => {
            buf.push(a);
            true
        }
        None => false,
    } {}
    args.push(buf);
    args
}

fn get_args<const ARG_COUNT: usize>(loc: &Location, func: &str, src: &str) -> [String; ARG_COUNT] {
    let mut args = get_all_args(loc, func, src).into_iter();

    core::array::from_fn(|i| {
        args.next()
            .unwrap_or_else(|| fatal_arg_count(loc, i, func))
            .to_string()
    })
}

fn main() -> Result<(), u32> {
    let mut args = std::env::args();

    let mut makefile_names = vec![
        "GNUmakefile".to_owned(),
        "makefile".to_owned(),
        "Makefile".to_owned(),
    ];

    let mut state = State::default();
    state.debug = matches!(std::env::var("IMAKE_DEBUG").as_ref().map(|x| x.as_str()), Ok("1"));
    
    let mut vars = HashMap::new();

    let mpath: String = args.next().unwrap().trim().into();
    state.basename = Path::new(&mpath)
        .file_name()
        .unwrap()
        .to_owned()
        .into_string()
        .unwrap();

    state.dirname = Path::new(&mpath).parent().unwrap().to_str().unwrap().into();

    let olddir: String = std::env::current_dir().unwrap().to_str().unwrap().into();
    state.curdir = olddir.clone();

    for (a, b) in std::env::vars() {
        vars.insert(
            a.clone(),
            Var::new(Flavor::Simple, Origin::Env, None, a, b, true),
        );
    }

    state.fullname = mpath.clone();
    let name: String = "MAKE".into();
    vars.insert(
        name.clone(),
        Var::new(
            Flavor::Simple,
            Origin::Default,
            None,
            name,
            mpath.clone(),
            true,
        ),
    );


    let n = "SHELL".to_string();
    vars.insert(
        n.clone(),
        Var::new(Flavor::Simple, Origin::Env, None, n, "/bin/sh".into(), true),
    );

    let n = ".SHELLFLAGS".to_string();
    vars.insert(
        n.clone(),
        Var::new(Flavor::Simple, Origin::Env, None, n, "-c".into(), true),
    );

    let n = "CC".to_string();
    vars.insert(
        n.clone(),
        Var::new(Flavor::Simple, Origin::Default, None, n, "cc".into(), true),
    );

    let level = std::env::var("MAKELEVEL")
        .ok()
        .unwrap_or_default()
        .parse::<u32>()
        .map_or(0, |x| x + 1)
        .to_string();

    let n = "MAKELEVELS".to_string();
    vars.insert(
        n.clone(),
        Var::new(Flavor::Simple, Origin::Env, None, n, level, true),
    );

    let mut makeflags = String::new();

    let mut dashC = false;

    while let Some(arg) = args.next() {
        let mut sargs = vec![];
        if arg.starts_with("--") {
            sargs.push(arg);
        } else if arg.starts_with("-") {
            let mut chars = arg.chars();
            chars.next(); // skip `-`
            for a in chars {
                sargs.push(String::from(a));
            }
        } else {
            sargs.push(arg);
        }
        let mut sargs = sargs.into_iter().peekable();
        while let Some(arg) = sargs.next() {
            match arg.as_str() {
                "b" | "m" => {
                    // Ignored for compatibilty.
                }
                "B" | "--always-make" => {
                    state.always_make = true;
                    makeflags.push('B');
                }
                "i" | "--ignore-errors" => {
                    state.ignore_errors = true;
                }
                s if s.starts_with("--directory=") => {}
                "C" => {
                    let dir = args.next().expect("no dir provided");
                    std::env::set_current_dir(Path::new(&dir)).unwrap();
                    state.curdir = std::env::current_dir().unwrap().to_str().unwrap().into();
                    dashC = true;
                }
                "v" | "--version" => {
                    println!("GNU Make 4.3 Compatible Iglunix Make");
                    return Ok(());
                }
                "f" => {
                    let n = args.next().expect("");
                    makefile_names = vec![n]
                }
                "s" | "--silent" | "--quiet" => {
                    state.silent = true;
                    makeflags.push('s');
                }
                "n" | "--just-print" | "--dry-run" | "--recon" => {
                    state.dryrun = true;
                }
                "k" | "--keep-going" => {
                    state.keep_going = true;
                }
                "--no-silent" => {
                    state.silent = false;
                }
                "--no-print-directory" => {
                    // TODO:
                }
                "j" => {
                    let mut n = String::new();
                    while match sargs.peek() {
                        Some(d) if d.parse::<usize>().is_ok() => {
                            n.extend(sargs.next().unwrap().chars());
                            true
                        }
                        _ => false,
                    } {}
                }
                "e" | "--environment-override" => {
                    // TODO:
                    // need some logic for var stuff to implement this
                    // sometimes we should store sometimes not
                }
                "" => {}
                a if !a.starts_with('-') => {
                    let mut l = String::new();
                    let mut is_var = false;
                    let mut v = String::new();

                    for c in a.chars() {
                        match c {
                            '=' => is_var = true,
                            a => {
                                if is_var {
                                    v.push(a)
                                } else {
                                    l.push(a)
                                }
                            }
                        }
                    }

                    if is_var {
                        vars.insert(
                            l.clone(),
                            Var::new(Flavor::Simple, Origin::CmdLine, None, l, v, false),
                        );
                    } else {
                        state.targets_to_make.push(l);
                    }
                }
                _ => return Err(1),
            }
        }
    }
    let name = "MAKEFLAGS".to_string();
    vars.insert(
        name.clone(),
        Var::new(
            Flavor::Simple,
            Origin::CmdLine,
            None,
            name,
            makeflags,
            false,
        ),
    );

    let makefile = makefile_names
        .into_iter()
        .find(|name| Path::new(&name).exists())
        .expect("No makefiles found")
        .clone();

    let mut leaving = None;

    if !state.silent && dashC {
        println!("{}: Entering directory '{}'", state.basename, state.curdir);
        leaving = Some(format!(
            "{}: Leaving directory '{}'",
            state.basename, state.curdir
        ));
    }

    let r = state_machine(state, vars, &makefile);

    if let Some(l) = leaving {
        eprintln!("{}", l);
    }

    r
}

#[derive(Default)]
struct ShellState {
    in_string: Option<char>,
}

fn process_for_shell(src: &str) -> String {
    // let mut out = String::new();
    // let mut state = ShellState::default();

    // for c in src.chars() {
    //     match (&mut state, c) {
    //         (ShellState { in_string, .. }, '\'') if in_string.is_none() => {
    //             *in_string = Some('\'');
    //         }
    //         (ShellState { in_string, .. }, '\'') if matches!(in_string, Some('\'')) => {
    //             *in_string = None;
    //         }
    //         (_, '#')  => {
    //             out.push('\\');
    //             out.push('#');
    //         }
    //         (_, a) => {
    //             out.push(a);
    //         }
    //     }
    // }

    // out
    src.to_owned()
}

/// Read a logical makefile line and discard after comment
fn read_logical_line(state: &State, file: &mut BufReader<File>, eof: &mut bool, line_no: &mut usize) -> String {
    let mut line: String = String::new();

    let mut needs_line = true;
    let mut discard = false;

    let mut just_spaces = true;

    while needs_line {
        let mut tmp_line = String::new();
        needs_line = false;
        // Handle end of file gracefully
        if matches!(file.read_line(&mut tmp_line), Ok(x) if x > 0) {
            *line_no += 1;

            if tmp_line.starts_with('#') {
                continue;
            }
            let mut chars = if line.is_empty() {
                tmp_line.chars().peekable()
            } else {
                tmp_line.trim().chars().peekable()
            };

            if matches!(chars.peek(), Some('\u{feff}')) {
                chars.next();
            }

            // we accept ' \t' gmake doesn't
            while just_spaces && matches!(chars.peek(), Some(' ')) {
                chars.next();
            }
            just_spaces = false;

            let mut sub_depth = 0;
            let mut in_quote = false;
            let mut in_dquote = false;
            while let Some(c) = chars.next() {
                match (in_quote, in_dquote, sub_depth, c) {
                    // (false, false, 0, '#') => discard = true,
                    (false, false, _, '$') => {
                        line.push('$');

                        match chars.peek() {
                            Some('(') => {
                                sub_depth += 1;
                                line.push('(');
                                chars.next();
                            }
                            Some('{') => {
                                sub_depth += 1;
                                line.push('{');
                                chars.next();
                            }
                            _ => {}
                        }
                    }

                    (false, false, _, a) if a == '}' || a == ')' => {
                        sub_depth -= 1;
                        line.push(a);
                    }

                    (a, false, _, '\'') => {
                        in_quote = !a;
                        line.push('\'');
                    }

                    (false, a, _, '"') => {
                        in_dquote = !a;
                        line.push('"');
                    }
                    (a, b, _, '\\') if a | b => {
                        match chars.peek() {
                            Some('\n') => {
                                needs_line = true;
                            },
                            _ => {}
                        }
                        line.push('\\');
                        line.push(chars.next().unwrap());
                    }
                    (false, false, _, '\\') => match chars.next() {
                        Some('\\') => line.push('\\'),
                        Some('\n') => needs_line = true,
                        _ => {}
                    },
                    (_, _, _, a) => {
                        //if !discard {
                            line.push(a);
                        //}
                    }
                }
            }
        } else {
            *eof = true;
        }
    }

    if state.debug {
        eprintln!("logical line: {}", line);
    }

    line
}

fn process_specials(state: &mut State, vars: &mut HashMap<String, Var>) {
    for t in &state.rules.clone() {
        if let Some(first_target) = t.targets.get(0) {
            match first_target.as_str() {
                ".SILENT" => {
                    if let RuleData::Prereq(_, prereqs) = &t.data {
                        let prereqs = expand_simple_ng(state, vars, &t.location, prereqs);
                        state
                            .silent_targets
                            .extend(prereqs.split_whitespace().map(|s| s.to_string()));
                    } else {
                        state.silent = true;
                    }
                }

                ".PHONY" => {
                    if let RuleData::Prereq(_, prereqs) = &t.data {
                        let prereqs = expand_simple_ng(state, vars, &t.location, prereqs);
                        state
                            .phony
                            .extend(prereqs.split_whitespace().map(|s| s.to_string()));
                    }
                }
                _ => {}
            }
        }
    }
}

/// setsup some options aswell
fn select_targets(state: &mut State, vars: &mut HashMap<String, Var>) -> Vec<String> {
    let mut best_matches = Vec::new();
    for t in &state.rules.clone() {
        let first_target = t.targets.get(0).map(|x| x.clone());
        let first_target = first_target.unwrap_or_default();
        match t {
            Rule {
                data: RuleData::Prereq(_, prereqs),
                ..
            } if first_target == ".DEFAULT" => {
                let prereqs = expand_simple_ng(state, vars, &t.location, prereqs);
                best_matches = prereqs.split_whitespace().map(|s| s.to_string()).collect();
            }

            Rule { .. } if first_target.starts_with('.') => {}
            _ => {
                if best_matches.is_empty() {
                    best_matches.push(first_target);
                }
            }
        }
    }
    best_matches
}

fn state_machine(mut state: State, mut vars: HashMap<String, Var>, file: &str) -> Result<(), u32> {
    process_lines(&mut state, &mut vars, file);

    process_specials(&mut state, &mut vars);

    build_graph(&mut state, &mut vars);

    let mut targets_to_make = state.targets_to_make.clone();

    if targets_to_make.is_empty() {
        targets_to_make = select_targets(&mut state, &mut vars)
    }

    for t in targets_to_make {
        // TODO:is here place to push var stack?
        let vars = vars.clone();
        if let Some((done_smth, has_recipies)) = process_target(&mut state, &vars, &t) {
            if !state.silent && !done_smth {
                if state.phony.contains(&t) || !has_recipies {
                    eprintln!("{}: Nothing to be done for '{}'.", state.basename, t);
                } else {
                    eprintln!("{}: '{}' is up to date.", state.basename, t);
                }
            }
        } else {
            eprintln!(
                "{}: *** No rule to make target '{}'.  Stop.",
                state.basename, t
            );
        }
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum Flavor {
    Undefined,
    Simple,
    Recursive,
}

#[derive(Debug, Clone, Copy)]
pub enum Origin {
    Undefined,
    Default,
    Env,
    EnvOverride,
    File,
    CmdLine,
    Override,
    Automatic,
}

#[derive(Debug, Clone)]
pub struct Var {
    flavor: Flavor,
    origin: Origin,
    loc: Option<Location>,
    name: String,
    value: String,
    exported: bool,
    unexported: bool,
    ex_exported: bool
}

impl Var {
    pub fn new(
        flavor: Flavor,
        origin: Origin,
        loc: Option<Location>,
        name: String,
        value: String,
        exported: bool,
    ) -> Self {
        let ret = Self {
            flavor,
            origin,
            loc,
            name,
            value,
            exported,
            unexported: false,
            ex_exported: false
        };
        ret.sync_env();
        ret
    }

    pub fn export(&mut self) {
        self.exported = true;
        self.ex_exported = true;
        self.sync_env();
    }

    pub fn unexport(&mut self) {
        self.exported = false;
        self.unexported = true;
        std::env::remove_var(&self.name);
    }

    fn sync_env(&self) {
        if self.exported {
            std::env::set_var(&self.name, &self.value);
        }
    }

    pub fn store(&mut self, value: String) {
        self.value = value;
        self.sync_env();
    }

    pub fn append(&mut self, value: &str) {
        self.value.push(' ');
        self.value.extend(value.trim().chars());
        self.sync_env();
    }

    fn eval(&self, state: &State, location: &Location, vars: &mut HashMap<String, Var>) -> String {
        // TODO: expand if recursive
        match self.flavor {
            Flavor::Recursive => expand_simple_ng(
                state,
                vars,
                self.loc.as_ref().unwrap_or(location),
                // TODO: errors should not use the var location but instead should use the line location
                // for errors
                //
                // location,
                &self.value,
            ),
            Flavor::Undefined | Flavor::Simple => self.value.clone(),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct Location {
    file_name: String,
    line: usize,
}

fn process_lines(state: &mut State, vars: &mut HashMap<String, Var>, file_name: &str) {
    #[derive(Debug, Clone, Copy)]
    enum VarOp {
        Store,
        Append,
    }

    #[derive(Debug)]
    enum Context {
        Unknown,
        Rule(String, Option<String>, Vec<String>),
        Var(VarOp, String),
    }

    let file = File::open(file_name).expect("can't find file");
    let mut file = BufReader::new(file);
    let mut eof = false;

    // Depth of false ifs. if we reach one if statement that's false this gets
    // incremented to 1. if we reach any other if statements whatever their outcome
    // this gets incremented. if we reach endifs this gets decremented until it's at 0
    // at which point we switch back to parsing things normally.
    let mut in_false = 0;

    // Only need to set this on the else in the true state.
    let mut found_true = false;

    // maybe need a depth like in_false here
    let mut in_define: Option<(String, Option<String>, String)> = None;

    let mut location = Location {
        file_name: file_name.into(),
        line: 0,
    };

    // TODO: .RECIPIEPREFIX
    let recipie_prefix = '\t';
    while !eof {
        let line = read_logical_line(state, &mut file, &mut eof, &mut location.line);
        // eprintln!("processing logical line: {}: in rule: {}", line.trim(), state.in_rule);
        //
        if let Some((v_name, op, buf)) = &mut in_define {
            if line.trim().starts_with("endef") {
                let v = vars.get(&v_name.to_string());
                if let Some(v) = v {
                    match op.as_ref().map(|x| x.as_str()) {
                        None | Some("=") => {
                            let v = vars.get_mut(v_name).unwrap();
                            v.store(buf.to_string());
                        }
                        Some(":=") | Some("::=") => {
                            let buf = expand_simple_ng(state, vars, &location, buf);
                            let v = vars.get_mut(&v_name.to_string()).unwrap();
                            v.store(buf.to_string());
                        }
                        Some("+=") => {
                            let buf = if matches!(v.flavor, Flavor::Simple) {
                                expand_simple_ng(state, vars, &location, buf)
                            } else {
                                buf.to_string()
                            };
                            let v = vars.get_mut(&v_name.to_string()).unwrap();
                            v.store(buf.to_string());

                        }
                        Some(_) => panic!()
                    }
                } else {
                    match op.as_ref().map(|x| x.as_str()) {
                        None | Some("=") | Some("+=") => {
                            vars.insert(v_name.clone(), Var::new(Flavor::Recursive, Origin::File, Some(location.clone()), v_name.clone(), buf.to_string(), false));
                        }
                        Some(":=") | Some("::=") => {
                            let buf = expand_simple_ng(state, vars, &location, buf);
                            vars.insert(v_name.clone(), Var::new(Flavor::Simple, Origin::File, Some(location.clone()), v_name.clone(), buf.to_string(), false));
                        }
                        Some(_) => panic!()
                    }

                }
                
                in_define = None;
            } else {
                buf.extend(line.chars());
            }
        } else if in_false > 0 {
            if line.trim().starts_with("ifdef ")
                || line.trim().starts_with("ifndef ")
                || line.trim().starts_with("ifeq ")
                || line.trim().starts_with("ifneq ")
            {
                in_false += 1;
            } else if line.trim().starts_with("endif") {
                in_false -= 1;


                
            } else if in_false == 1 && !found_true && line.trim().starts_with("else") {
                let line = line.trim()[4..].trim();
                if line.len() == 0 {
                    in_false = 0;
                } else if line.trim().starts_with("ifeq ") {
                    let s_args = line.trim()[5..].trim().to_string();
                    let len = s_args.len();
                    let mut args = s_args.chars().peekable();
                    let mut args: Box<dyn Iterator<Item = _>> = if *args.peek().unwrap() == '(' {
                        Box::new(s_args[1..(len - 1)].split(','))
                    } else {
                        Box::new(s_args.split_whitespace())
                    };
                    let a1 = args.next().unwrap();
                    let a2 = args.next().unwrap();
                    let a1 = expand_simple_ng(state, vars, &location, &a1).replace(['"', '\''], "");
                    let a2 = expand_simple_ng(state, vars, &location, &a2).replace(['"', '\''], "");
                    if a1.trim() == a2.trim() {
                        in_false = 0;
                    }
                } else if line.trim().starts_with("ifneq ") {
                    let s_args = line.trim()[6..].trim().to_string();
                    let len = s_args.len();
                    let mut args = s_args.chars().peekable();
                    let mut args: Box<dyn Iterator<Item = _>> = if *args.peek().unwrap() == '(' {
                        Box::new(s_args[1..(len - 1)].split(','))
                    } else {
                        Box::new(s_args.split_whitespace())
                    };
                    let a1 = args.next().unwrap();
                    let a2 = args.next().unwrap();
                    let a1 = expand_simple_ng(state, vars, &location, &a1).replace(['"', '\''], "");
                    let a2 = expand_simple_ng(state, vars, &location, &a2).replace(['"', '\''], "");
                    if a1.trim() != a2.trim() {
                        in_false = 0;
                    }
                } else if line.trim().starts_with("ifdef") {
                    let var = line.trim()[6..].trim();
                    let var = expand_simple_ng(state, vars, &location, &var);

                    if vars.contains_key(&var) {
                        in_false = 0;
                    }
                } else if line.trim().starts_with("ifndef ") {
                    let var = line.trim()[7..].trim();
                    let var = expand_simple_ng(state, vars, &location, &var);

                    if !vars.contains_key(&var) {
                        in_false = 0;
                    }
                }
            }
        } else {
            match line {
                l if l.starts_with(recipie_prefix) && state.in_rule => {
                    let r = match state.rules.last() {
                        Some(Rule {
                            targets,
                            data: RuleData::Prereq(..),
                            ..
                        })
                        | Some(Rule {
                            targets,
                            data: RuleData::Recipie(..),
                            ..
                        }) => Rule {
                            location: location.clone(),
                            targets: targets.clone(),
                            data: RuleData::Recipie(l),
                        },

                        t => panic!("{:#?}:{}", t, l),
                    };
                    state.rules.push(r);
                }
                l if l.starts_with(recipie_prefix) && !state.in_rule => {
                    panic!("Not currently within a rule {}", l);
                }
                l if l.trim().is_empty() => {
                    // do nothing on empty lines that don't start with rule prefix
                    // state.in_rule = false;
                }
                l if l.starts_with("include ") => {
                    state.in_rule = false;

                    process_lines(state, vars, &l[8..].trim());
                }
                l if l.trim().starts_with("ifeq ") => {
                    let s_args = l.trim()[5..].trim().to_string();
                    let len = s_args.len();
                    let mut args = s_args.chars().peekable();
                    let mut args: Box<dyn Iterator<Item = _>> = if *args.peek().unwrap() == '(' {
                        Box::new(s_args[1..(len - 1)].split(','))
                    } else {
                        Box::new(s_args.split_whitespace())
                    };
                    let a1 = args.next().unwrap();
                    let a2 = args.next().unwrap();
                    let a1 = expand_simple_ng(state, vars, &location, &a1).replace(['"', '\''], "");
                    let a2 = expand_simple_ng(state, vars, &location, &a2).replace(['"', '\''], "");
                    if a1.trim() != a2.trim() {
                        in_false += 1
                    }
                }
                l if l.trim().starts_with("ifneq ") => {
                    let s_args = l.trim()[5..].trim().to_string();
                    let len = s_args.len();
                    let mut args = s_args.chars().peekable();
                    let mut args: Box<dyn Iterator<Item = _>> = if *args.peek().unwrap() == '(' {
                        Box::new(s_args[1..(len - 1)].split(','))
                    } else {
                        Box::new(s_args.split_whitespace())
                    };
                    let a1 = args.next().unwrap();
                    let a2 = args.next().unwrap();
                    let a1 = expand_simple_ng(state, vars, &location, &a1).replace(['"', '\''], "");
                    let a2 = expand_simple_ng(state, vars, &location, &a2).replace(['"', '\''], "");
                    if a1.trim() == a2.trim() {
                        in_false += 1
                    }
                }
                l if l.trim().starts_with("ifdef ") => {
                    let var = l.trim()[6..].trim();
                    let var = expand_simple_ng(state, vars, &location, &var);
                    if !vars.contains_key(&var) {
                        in_false += 1
                    }
                }
                l if l.trim().starts_with("ifndef ") => {
                    let var = l.trim()[7..].trim();
                    let var = expand_simple_ng(state, vars, &location, &var);
                    if vars.contains_key(&var) {
                        in_false += 1
                    }
                }
                l if l.trim().starts_with("endif") => {
                    // TODO: in_true?
                }
                l if l.trim().starts_with("else") => {
                    found_true = true;
                    in_false += 1;
                }
                l if l.starts_with("-include ") | l.starts_with("sinclude ") => {
                    state.in_rule = false;
                    if Path::new(l[8..].trim()).exists() {
                        process_lines(state, vars, &l[8..].trim());
                    }
                }
                l if l.trim().starts_with("define ") => {
                    let mut args = l.split_whitespace();
                    let _define = args.next().unwrap();
                    let v_name = args.next().unwrap();
                    let op = args.next();

                    in_define = Some((v_name.into(), op.map(|x| x.into()), String::new()));
                }
                l => parse_line(state, vars, &location, &l),
            }
        }
    }
}

// TODO: rule execution handling
// (inference rules come later)
//
//
// Start by processing a target to build:
//  - Create a new rule structure, process all rules in the file; append any
//    rule specific varaibles and prerequisites.
//
//  - Loop over prerequisites and process all of them in the same way.
//    check if they fit inference rules and append that information to that
//    rules structure.
//
//  - once all prerequisites have been processed execute the rule.

#[derive(Debug, Clone)]
struct Rule {
    location: Location,
    targets: Vec<String>,
    data: RuleData,
}

#[derive(Debug, Clone, Copy)]
enum VarOp {
    /// expand or not
    Store(bool),
    Append,
    StoreIfUndef,
    Shell,
}

#[derive(Debug, Clone)]
enum RuleData {
    Prereq(bool, String),
    Var(String, VarOp, String),
    Recipie(String),
}

/// All the rules for a single target bundled together for processing
/// expansion of recipies
#[derive(Debug, Clone, Default)]
struct TargetRule {
    target: String,
    vars: HashMap<String, String>,
    prerequisites: Vec<String>,
}

fn build_graph(state: &mut State, vars: &HashMap<String, Var>) {
    enum RuleType {
        Implicit,
        Phony,
        File
    }
    // types of rules
    //
    //  - add a prereq (these should all be resolved)
    //
    #[derive(Debug, Clone, Default)]
    struct GraphEntry {
        rule_name: String,
        // List of prerequisites. If a prerequisite is a file
        // not created by any target. Then graph[i]
        prereqs: Vec<String>,
        phony: bool,
        recipies: Vec<String>,
        vars: Vec<Var>
    }

    // Vec for double colons
    let mut str_lut = HashMap::<String, Vec<usize>>::new();
    
    let mut graph = Vec::<GraphEntry>::new();
    for rule in &state.rules{
        match rule {
            Rule { targets, data: RuleData::Prereq(double_colon, prereq), .. } => {
                for target in targets {
                    match str_lut.get_mut(target) {
                        Some(target) if !double_colon => {
                            graph[target[0]].prereqs.extend(prereq.split_whitespace().map(|x| x.to_string()));
                        }
                        Some(target_ids) if *double_colon => {
                            target_ids.push(graph.len());
                            graph.push(GraphEntry {
                                rule_name: target.to_string(),
                                prereqs: prereq.split_whitespace().map(|x| x.to_string()).collect(),
                                phony: false,
                                recipies: Vec::new(),
                                vars: Vec::new()
                            });
                        }
                        Some(_) => unreachable!(),
                        None => {
                            str_lut.insert(target.to_string(), vec![graph.len()]);
                            graph.push(GraphEntry {
                                rule_name: target.to_string(),
                                prereqs: prereq.split_whitespace().map(|x| x.to_string()).collect(),
                                phony: false,
                                recipies: Vec::new(),
                                vars: Vec::new()
                            });
                        }
                    }
                }
            }
            Rule { targets, data: RuleData::Recipie(recipie), .. } => {
                for target in targets {
                    match str_lut.get_mut(target) {
                        Some(target) => {
                            graph[target[target.len() - 1]].recipies.push(recipie.to_string());
                        }
                        None => {
                            panic!();
                            // TODO: unreachable!()
                        }
                    }
                }
            }
            Rule { targets, data: RuleData::Var(lhs, op, rhs), .. } => {
                for target in targets {
                    match str_lut.get_mut(target) {
                        Some(target) => {

                        }
                        None => {}
                    }
                }
            }
            _ => ()
        }
    }

    if state.debug {
        eprintln!("{:#?}", graph);
    }
}

fn process_target(
    state: &mut State,
    vars: &HashMap<String, Var>,
    name: &str,
) -> Option<(bool, bool)> {
    let mut done_smth = false;
    let mut vars = vars.clone();
    vars.insert(
        "@".into(),
        Var::new(
            Flavor::Simple,
            Origin::Automatic,
            None,
            "@".into(),
            name.into(),
            false,
        ),
    );

    if state.processed.contains(&name.to_string()) {
        return Some((false, false));
    } else {
        state.processed.push(name.to_string());
    }

    let mut target_rule = TargetRule::default();
    target_rule.target = name.to_owned();

    let mut recipies = Vec::new();

    let mut prereqs_var = Var::new(
        Flavor::Simple,
        Origin::Automatic,
        None,
        "?".into(),
        "".into(),
        false,
    );

    let mut was_prereq = false;
    let mut was_recipies = false;
    let mut found_rules = false;

    let mut was_single = false;
    let mut was_double = false;

    for rule in &state.rules.clone() {
        if rule.targets.contains(&name.to_owned()) {
            found_rules |= true;
            match &rule.data {
                RuleData::Var(a, _op, b) => {
                    target_rule.vars.insert(a.into(), b.into());
                    was_prereq = false;
                    was_recipies = false;
                }
                RuleData::Prereq(a, prereqs) => {
                    // let prereqs = expand_simple_ng(state, &mut vars, &rule.location, prereqs);
                    if *a && was_single {
                        fatal_double_and_single(&rule.location, name);
                    } else if !*a && was_double {
                        fatal_double_and_single(&rule.location, name);
                    } else if *a {
                        was_double = true;
                    } else {
                        was_single = true;
                    }

                    prereqs_var.append(&prereqs);

                    target_rule
                        .prerequisites
                        .extend(prereqs.split_whitespace().map(|s| s.to_string()));
                    was_prereq = true;
                    was_recipies = false;
                }
                RuleData::Recipie(r) => {
                    if !recipies.is_empty() && !was_recipies {
                        if !was_prereq {
                            panic!();
                        } else if !was_double {
                            recipies = Vec::new();
                        }
                    }
                    was_recipies = true;
                    was_prereq = false;
                    recipies.push((rule.location.clone(), r.clone()));
                }
            }
        }
    }

    vars.insert("?".into(), prereqs_var.clone());
    prereqs_var.name = "<".into();
    vars.insert("<".into(), prereqs_var);

    for t in &target_rule.prerequisites {
        if let Some((a, ..)) = process_target(state, &vars, t) {
            done_smth |= a;
        } else if !state.phony.contains(&t.trim().to_string()) {
            println!(
                "{}: *** No rule to make target '{}', needed by '{}'. Stop",
                state.basename, t, name
            );
            std::process::exit(130);
        }
    }

    let path = Path::new(name);
    let mut needs_updating = false;
    if state.phony.contains(&name.to_string()) {
        needs_updating = true;
    } else if let Ok(Ok(time)) = path.metadata().map(|m| m.modified()) {
        for p in &target_rule.prerequisites {
            if state.phony.contains(p) {
                needs_updating = true;
                // phony targets always exist
                found_rules = true;
            } else {
                let ptime = Path::new(&p).metadata().map(|m| m.modified());

                if let Ok(Ok(ptime)) = ptime {
                    if ptime > time {
                        needs_updating = true;
                    }
                } else {
                    needs_updating = true;
                }
            }
        }
    } else {
        needs_updating = true;
    }

    if !found_rules && needs_updating {
        return None;
    }

    let mut has_recipies = false;

    if needs_updating {
        let mut expanded = Vec::new();

        for (loc, r) in &recipies {
            let cmd = expand_simple_ng(state, &mut vars, loc, r);

            let cmd = cmd.trim();

            if !cmd.is_empty() {
                expanded.push((loc.clone(), cmd.to_string()));
            }
        }

        has_recipies = !expanded.is_empty();

        for (loc, cmd) in &expanded {
            done_smth = true;

            let mut cmd = cmd.as_str();
            let ignore_errors = if cmd.starts_with('-') {
                cmd = &cmd[1..];
                true
            } else {
                // TODO: state.ignore errors
                state.ignore_errors
            };

            let mut silent = state.silent_targets.contains(&name.to_string());

            if cmd.starts_with('@') {
                cmd = &cmd[1..];
                silent = true;
            }

            if (!silent || state.dryrun) && !state.silent {
                println!("{}", cmd);
            }

            // TODO: a dirty state tracker
            let shell = if let Some(v) = vars.get("SHELL") {
                v.clone().eval(state, loc, &mut vars)
            } else {
                String::new()
            };

            let shell_flags = if let Some(v) = vars.get(".SHELLFLAGS") {
                v.clone().eval(state, loc, &mut vars)
            } else {
                String::new()
            };

            let cmd_name = cmd.trim().split_ascii_whitespace().next().unwrap();
            // WONTFIX: we will not check if a program we're executing exists before
            // hand. we will not do a special printy thing.
            //
            // WONTFIX: gmake and bmake do internal processing if the shell is `/bin/sh` we will not

            let mut leaving = None;

            // std::env::set_var(
            //     "MAKELEVEL",
            //     (vars.get("MAKELEVEL")
            //         .unwrap_or_default()
            //         .value
            //         .parse::<u32>()
            //         .unwrap()
            //         + 1)
            //     .to_string(),
            // );

            if !silent && cmd_name == state.fullname {
                println!(
                    "{}[1]: Entering directory '{}'",
                    state.basename, state.curdir
                );
                leaving = Some(format!(
                    "{}[1]: Leaving directory '{}'",
                    state.basename, state.curdir
                ));
            } else {
            }

            let status = Command::new(shell)
                .arg0(&state.basename)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .arg(shell_flags)
                .arg(cmd)
                .status()
                .expect("command failed");
            if !status.success() {
                if ignore_errors {
                    eprintln!(
                        "{}: [{}:{}: {}] Error {} (ignored)",
                        state.basename,
                        loc.file_name,
                        loc.line,
                        name,
                        status.code().unwrap_or_default()
                    );
                } else {
                    eprintln!(
                        "{}: *** [{}:{}: {}] Error {}",
                        state.basename,
                        loc.file_name,
                        loc.line,
                        name,
                        status.code().unwrap_or_default()
                    );
                    if !state.keep_going {
                        std::process::exit(2);
                    }
                }
            } else if let Some(s) = leaving {
                println!("{}", s);
            }
        }
    }

    Some((done_smth, has_recipies))
}

// TODO: symbol table
// Need a proper symbol table that keeps track of variable flavors, expands only when needed,
// and updates the environment.
//
// Perhaps scopes are needed
//
// TODO: process launching utilities

/// Keep track of defined variables
struct SymbolTable {}

impl SymbolTable {
    pub fn new() -> Self {
        Self {}
    }

    pub fn set(name: &str, value: &str) {
        std::env::set_var(name, value)
    }

    pub fn get(name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
}

fn expand_ng(
    state: &State,
    vars: &mut HashMap<String, Var>,
    loc: &Location,
    src: &mut String,
) -> String {
    #[derive(Debug)]
    enum SubType {
        Var,
        Info,
        Shell,
        Subst,
        Warn,
        BaseName,
        AddPrefix,
        AddSuffix,
        Sort,
        FirstWord,
        LastWord,
        Words,
        Suffix,
        Join,
        Dir,
        NotDir,
        AbsPath,
        FindString,
        Error,
        Call,
        Flavor,
        Origin,
        ForEach,
        Word,
        WordList,
        PatSubst,
        SubstRef,
        Strip,
        WildCard,
        Value
    }

    #[cfg(debug_assertions)]
    let esrc = Some(src.clone());

    #[cfg(not(debug_assertions))]
    let esrc = None;

    // `$` should have already been consumed
    let x = src.pop();
    match x {
        Some(b) if (b == '(') || (b == '{') => {
            let mut arg = String::new();
            let mut func = SubType::Var;
            let mut had_space = false;

            let mut delim_stack = b.to_string();

            // keep track if we hit delimiters for substitutions X:a=b
            let mut hit_colon = true;
            let mut defo_subst = false;
            while !delim_stack.is_empty() {
                let c = src.pop().expect(&format!(
                    "aaaa should handle this $(... without the ): {}: {}: {}",
                    arg,
                    src,
                    esrc.clone().unwrap_or_default()
                ));
                arg.push(c);
                match c {
                    ')' if delim_stack.chars().last().unwrap() == '(' => {
                        delim_stack.pop();
                    }
                    '}' if delim_stack.chars().last().unwrap() == '{' => {
                        delim_stack.pop();
                    }
                    '}' if delim_stack.chars().last().unwrap() == '(' => fatal_unterm_var(loc),
                    ')' if delim_stack.chars().last().unwrap() == '{' => fatal_unterm_var(loc),
                    '(' => delim_stack.push('('),
                    '{' => delim_stack.push('{'),
                    ':' if delim_stack.len() == 1 => {
                        hit_colon = true;
                    }
                    '=' if delim_stack.len() == 1 && hit_colon => {
                        defo_subst = true;
                    }

                    ' ' if delim_stack.len() == 1 && !had_space => {
                        had_space = true;
                        func = match arg.trim() {
                            "info" => {
                                arg = String::new();
                                SubType::Info
                            }
                            "shell" => {
                                arg = String::new();
                                SubType::Shell
                            }
                            "subst" => {
                                arg = String::new();
                                SubType::Subst
                            }
                            "warning" => {
                                arg = String::new();
                                SubType::Warn
                            }
                            "basename" => {
                                arg = String::new();
                                SubType::BaseName
                            }
                            "addprefix" => {
                                arg = String::new();
                                SubType::AddPrefix
                            }
                            "addsuffix" => {
                                arg = String::new();
                                SubType::AddSuffix
                            }
                            "sort" => {
                                arg = String::new();
                                SubType::Sort
                            }
                            "firstword" => {
                                arg = String::new();
                                SubType::FirstWord
                            }
                            "lastword" => {
                                arg = String::new();
                                SubType::LastWord
                            }
                            "words" => {
                                arg = String::new();
                                SubType::Words
                            }
                            "word" => {
                                arg = String::new();
                                SubType::Word
                            }
                            "wordlist" => {
                                arg = String::new();
                                SubType::WordList
                            }
                            "suffix" => {
                                arg = String::new();
                                SubType::Suffix
                            }
                            "join" => {
                                arg = String::new();
                                SubType::Join
                            }
                            "notdir" => {
                                arg = String::new();
                                SubType::NotDir
                            }
                            "dir" => {
                                arg = String::new();
                                SubType::Dir
                            }
                            "abspath" => {
                                arg = String::new();
                                SubType::AbsPath
                            }
                            "findstring" => {
                                arg = String::new();
                                SubType::FindString
                            }
                            "error" => {
                                arg = String::new();
                                SubType::Error
                            }
                            "call" => {
                                arg = String::new();
                                SubType::Call
                            }
                            "flavor" => {
                                arg = String::new();
                                SubType::Flavor
                            }
                            "origin" => {
                                arg = String::new();
                                SubType::Origin
                            }
                            "foreach" => {
                                arg = String::new();
                                SubType::ForEach
                            }
                            "patsubst" => {
                                arg = String::new();
                                SubType::PatSubst
                            }
                            "strip" => {
                                arg = String::new();
                                SubType::Strip
                            }
                            "wildcard" => {
                                arg = String::new();
                                SubType::WildCard
                            }
                            "value" => {
                                arg = String::new();
                                SubType::Value
                            }
                            _ => SubType::Var,
                        };
                    }
                    _ => {}
                }
            }
            arg.pop(); // drop last `)` or `}`

            if matches!(func, SubType::Var) && defo_subst {
                func = SubType::SubstRef
            }

            // TODO: fill in expand stuff
            match func {
                SubType::Var => {
                    let name = expand_simple_ng(state, vars, loc, arg.trim());
                    if let Some(v) = vars.get(&name) {
                        v.clone().eval(state, loc, vars)
                    } else {
                        String::new()
                    }
                }
                SubType::Shell => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let cmd = process_for_shell(&arg);

                    let cmd_name = cmd.split_whitespace().next().unwrap();

                    // WONTFIX: gnu make does internal interpreting of shell
                    // we will not do this and let the shell handle everything
                    //
                    // let cnf_status = Command::new("/bin/sh")
                    //     .arg0(&state.basename)
                    //     .stdout(Stdio::null())
                    //     .stderr(Stdio::null())
                    //     .arg("-c")
                    //     .arg(format!("command -V {}", cmd_name))
                    //     .status()
                    //     .expect("command failed");
                    // if !cnf_status.success() {
                    //     eprintln!(
                    //         "{}: {}: No such file or directory",
                    //         state.basename, cmd_name
                    //     );
                    //     let name: String = ".SHELLSTATUS".into();
                    //     // TODO: move vars out of state
                    //     // vars.insert(
                    //     //     name.clone(),
                    //     //     Var::new(Flavor::Simple, Origin::Env, name, "127".into(), false),
                    //     // );
                    //     String::new()
                    // } else {
                    // }
                    let shell = vars
                        .get("SHELL")
                        .expect("shell must be defined to execute stuff");
                    let shell = shell.clone().eval(state, loc, vars);

                    let shell_flags = vars.get(".SHELLFLAGS").unwrap();
                    let shell_flags = shell_flags.clone().eval(state, loc, vars);

                    let out = Command::new(shell)
                        .arg0(&state.basename)
                        .args(shell_flags.split_ascii_whitespace())
                        .arg(cmd)
                        .output()
                        .expect("Command failed to execute");
                    let s = String::from_utf8(out.stdout).unwrap();

                    let name: String = ".SHELLSTATUS".into();
                    vars.insert(
                        name.clone(),
                        Var::new(
                            Flavor::Simple,
                            Origin::Env,
                            Some(loc.clone()),
                            name,
                            format!("{}", out.status.code().unwrap_or_default()),
                            false,
                        ),
                    );
                    s
                }
                SubType::Info => {
                    println!("{}", expand_simple_ng(state, vars, loc, &arg));
                    String::new()
                }

                SubType::Subst => {
                    let mut args = arg.split(",");
                    let from = args.next().unwrap();
                    let from = expand_simple_ng(state, vars, loc, &from);
                    let to = args.next().unwrap();
                    let to = expand_simple_ng(state, vars, loc, &to);
                    let text = args.next().unwrap();
                    let text = expand_simple_ng(state, vars, loc, &text);
                    text.replace(&from, &to)
                }
                SubType::Warn => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    eprintln!("{}:{}: {}", loc.file_name, loc.line, arg);
                    String::new()
                }
                SubType::BaseName => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let names = arg.split_whitespace().rev();
                    let mut out = String::new();
                    for name in names {
                        let mut rev = name.chars().rev().peekable();
                        let mut purged = String::new();
                        let mut no_dot = false;
                        while match rev.peek() {
                            Some('.') => {
                                rev.next();
                                false
                            }
                            Some('/') => {
                                no_dot = true;
                                false
                            }
                            Some(_) => {
                                purged.push(rev.next().unwrap_or_else(|| unreachable!()));
                                true
                            }
                            None => {
                                no_dot = true;
                                false
                            }
                        } {}
                        if no_dot {
                            out.extend(purged.chars());
                        }
                        out.extend(rev);
                        out.push(' ');
                    }
                    out.chars().rev().collect()
                }
                SubType::Suffix => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let names = arg.split_whitespace().rev();
                    let mut out = String::new();
                    for name in names {
                        let mut rev = name.chars().rev().peekable();
                        let mut purged = String::new();
                        let mut no_dot = false;
                        while match rev.peek() {
                            Some('/') => {
                                no_dot = true;
                                false
                            }
                            Some(&a) => {
                                purged.push(rev.next().unwrap_or_else(|| unreachable!()));
                                a != '.'
                            }
                            None => {
                                no_dot = true;
                                false
                            }
                        } {}
                        if !no_dot {
                            out.extend(purged.chars());
                        }
                        out.push(' ');
                    }
                    out.chars().rev().collect()
                }
                SubType::AddPrefix => {
                    let mut args = arg.split(",");
                    let prefix = args.next().unwrap();
                    let prefix = expand_simple_ng(state, vars, loc, &prefix);
                    let args = args.next().unwrap();
                    let args = expand_simple_ng(state, vars, loc, &args);
                    args.split_whitespace()
                        .map(|x| format!("{}{}", prefix, x))
                        .fold(String::new(), |s, x| format!("{} {}", s, x))
                }
                SubType::AddSuffix => {
                    let mut args = arg.split(",");
                    let suffix = args.next().unwrap();
                    let suffix = expand_simple_ng(state, vars, loc, &suffix);
                    let args = args.next().unwrap();
                    let args = expand_simple_ng(state, vars, loc, &args);
                    args.split_whitespace()
                        .map(|x| format!("{}{}", x, suffix))
                        .fold(String::new(), |s, x| format!("{} {}", s, x))
                }
                SubType::Sort => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let mut args = arg.split_whitespace().collect::<Vec<_>>();
                    args.sort();
                    args.dedup();
                    let mut out = String::new();
                    for arg in args.into_iter() {
                        out.extend(arg.chars());
                        out.push(' ');
                    }
                    out
                }
                SubType::FirstWord => expand_simple_ng(state, vars, loc, &arg)
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                SubType::LastWord => expand_simple_ng(state, vars, loc, &arg)
                    .split_whitespace()
                    .last()
                    .unwrap_or_default()
                    .to_string(),
                SubType::Words => expand_simple_ng(state, vars, loc, &arg)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .len()
                    .to_string(),
                SubType::Join => {
                    let mut args = arg.split(',');
                    let a1 = args.next().unwrap();
                    let a1 = expand_simple_ng(state, vars, loc, &a1);
                    let a1 = a1.split_whitespace();
                    let a2 = args.next().unwrap();
                    let a2 = expand_simple_ng(state, vars, loc, &a2);
                    let a2 = a2.split_whitespace();
                    let mut out = String::new();
                    for (a, b) in a1.zip(a2) {
                        out.extend(a.chars());
                        out.extend(b.chars());
                        out.push(' ');
                    }
                    out
                }
                SubType::NotDir => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let names = arg.split_whitespace().rev();
                    let mut out = String::new();
                    for name in names {
                        let mut rev = name.chars().rev().peekable();
                        let mut purged = String::new();
                        while match rev.peek() {
                            Some('/') => false,
                            Some(_) => {
                                purged.push(rev.next().unwrap());
                                true
                            }
                            None => false,
                        } {}
                        out.extend(purged.chars());
                        out.push(' ');
                    }
                    out.chars().rev().collect()
                }
                SubType::Dir => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let names = arg.split_whitespace().rev();
                    let mut out = String::new();
                    for name in names {
                        let mut rev = name.chars().rev().peekable();
                        let mut purged = String::new();
                        let mut no_slash = false;
                        while match rev.peek() {
                            Some('/') => false,
                            Some(_) => {
                                purged.push(rev.next().unwrap());
                                true
                            }
                            None => {
                                no_slash = true;
                                false
                            }
                        } {}
                        if no_slash {
                            out.push('/');
                            out.push('.');
                        } else {
                            out.extend(rev);
                        }
                        out.push(' ');
                    }
                    out.chars().rev().collect()
                }
                SubType::AbsPath => expand_simple_ng(state, vars, loc, &arg)
                    .split_whitespace()
                    .map(|x| {
                        Path::new(x)
                            .canonicalize()
                            .map(|x| x.to_str().unwrap().to_string())
                            .unwrap_or_default()
                    })
                    .fold(String::new(), |s, x| format!("{} {}", s, x)),
                SubType::FindString => {
                    let mut args = arg.split(',');
                    let s = args.next().unwrap();
                    let s = expand_simple_ng(state, vars, loc, &s);
                    let rhs = args.next().unwrap();
                    let rhs = expand_simple_ng(state, vars, loc, &rhs);
                    if rhs.contains(&s) {
                        s.into()
                    } else {
                        String::new()
                    }
                }
                SubType::Error => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    eprintln!("{}:{}: *** {}.  Stop.", loc.file_name, loc.line, arg.trim());
                    std::process::exit(2);
                }
                SubType::Call => {
                    let args = get_all_args(loc, "call", &arg);
                    let mut args = args.into_iter();
                    let name = args.next().unwrap();
                    let name = expand_simple_ng(state, vars, loc, &name.trim());
                    let mut vars = vars.clone();
                    let mut highest = 0;
                    for (i, arg) in args.enumerate() {
                        let arg = expand_simple_ng(state, &mut vars, loc, &arg);
                        highest = i + 2;
                        let n = (i + 1).to_string();
                        vars.insert(
                            n.clone(),
                            Var::new(
                                Flavor::Simple,
                                Origin::File,
                                Some(loc.clone()),
                                n,
                                arg.to_string(),
                                false,
                            ),
                        );
                    }
                    // TODO: hack. needs to be sorted out in a refactor.
                    // need a better data structure for storing vars.
                    for i in highest..100 {
                        vars.remove(&i.to_string());
                    }
                    
                    if let Some(v) = vars.get(&name) {
                        let v = v.clone();
                        v.clone().eval(state, loc, &mut vars)
                    } else {
                        String::new()
                    }
                }
                SubType::Flavor => {
                    let name = arg.trim();
                    let name = expand_simple_ng(state, vars, loc, name);
                    match vars.get(&name) {
                        Some(Var {
                            flavor: Flavor::Simple,
                            ..
                        }) => "simple",
                        Some(Var {
                            flavor: Flavor::Recursive,
                            ..
                        }) => "recursive",
                        Some(Var {
                            flavor: Flavor::Undefined,
                            ..
                        })
                        | None => "undefined",
                    }
                    .into()
                }
                SubType::Origin => {
                    let name = arg.trim();
                    let name = expand_simple_ng(state, vars, loc, name);
                    match vars.get(&name) {
                        Some(Var {
                            origin: Origin::Default,
                            ..
                        }) => "default".into(),
                        Some(Var {
                            origin: Origin::Env,
                            ..
                        }) => "environment".into(),
                        Some(Var {
                            origin: Origin::EnvOverride,
                            ..
                        }) => "environment override".into(),
                        Some(Var {
                            origin: Origin::File,
                            ..
                        }) => "file".into(),
                        Some(Var {
                            origin: Origin::CmdLine,
                            ..
                        }) => "command line".into(),
                        Some(Var {
                            origin: Origin::Override,
                            ..
                        }) => "override".into(),
                        Some(Var {
                            origin: Origin::Automatic,
                            ..
                        }) => "automatic".into(),
                        Some(Var {
                            origin: Origin::Undefined,
                            ..
                        })
                        | None => "undefined".into(),
                    }
                }
                SubType::ForEach => {
                    let mut args = get_args::<3>(loc, "foreach", &arg);
                    args[0] = expand_simple_ng(state, vars, loc, &args[0]);
                    args[1] = expand_simple_ng(state, vars, loc, &args[1]);
                    let mut vars = vars.clone();

                    let mut out = String::new();

                    for v in args[1].split_whitespace() {
                        vars.insert(
                            args[0].trim().into(),
                            Var::new(
                                Flavor::Simple,
                                Origin::File,
                                Some(loc.clone()),
                                args[0].trim().into(),
                                v.to_string(),
                                false,
                            ),
                        );

                        out.extend(expand_simple_ng(state, &mut vars, loc, &args[2]).chars());
                        out.push(' ');
                    }
                    out.pop();

                    out
                }
                SubType::Word => {
                    let mut args = get_args::<2>(loc, "words", &arg);
                    args[0] = expand_simple_ng(state, vars, loc, &args[0]);
                    args[1] = expand_simple_ng(state, vars, loc, &args[1]);
                    let n = args[0].trim().parse::<usize>().unwrap_or_else(|_| {
                        println!(
                            "{}:{}: *** non-numeric first argument to 'word' function: '{}'.  Stop.",
                            loc.file_name, loc.line, args[0]
                        );
                        std::process::exit(2)
                    });
                    let mut words = args[1].split_whitespace();

                    if n == 0 {
                        println!("{}:{}: *** first argument to 'word' function must be greater than 0.  Stop.", loc.file_name, loc.line);
                        std::process::exit(2)
                    }

                    words.nth(n - 1).unwrap_or_default().to_string()
                }
                SubType::WordList => {
                    let mut args = get_args::<3>(loc, "wordlist", &arg);
                    args[0] = expand_simple_ng(state, vars, loc, &args[0]);
                    args[1] = expand_simple_ng(state, vars, loc, &args[1]);
                    args[2] = expand_simple_ng(state, vars, loc, &args[2]);
                    let mut n = args[0].trim().parse::<usize>().unwrap_or_else(|_| {
                        println!(
                            "{}:{}: *** non-numeric first argument to 'wordlist' function: '{}'.  Stop.",
                            loc.file_name, loc.line, args[0]
                        );
                        std::process::exit(2)
                    });
                    let mut e = args[1].trim().parse::<usize>().unwrap_or_else(|_| {
                        println!(
                            "{}:{}: *** non-numeric second argument to 'wordlist' function: '{}'.  Stop.",
                            loc.file_name, loc.line, args[1]
                        );
                        std::process::exit(2)
                    });

                    if n == 0 {
                        println!(
                            "{}:{}: *** invalid first argument to 'wordlist' function: '0'.  Stop.",
                            loc.file_name, loc.line
                        );
                        std::process::exit(2)
                    }
                    // i was incorrect here it doesn't get reversed
                    let rev = n > e;
                    let rev = false;

                    let words = args[2].split_whitespace().collect::<Vec<_>>();
                    let out_words = if rev {
                        &words[std::cmp::min(e - 1, words.len())..std::cmp::min(n, words.len())]
                    } else {
                        &words[std::cmp::min(n - 1, words.len())..std::cmp::min(e, words.len())]
                    };
                    let out_words = out_words
                        .into_iter()
                        .map(|x| format!("{} ", x))
                        .collect::<Vec<_>>();
                    if rev {
                        out_words.into_iter().rev().collect::<String>()
                    } else {
                        out_words.into_iter().collect::<String>()
                    }
                }
                SubType::SubstRef => {
                    let (var, rhs) = arg.split_once(':').unwrap();
                    let (lhs, rhs) = rhs.split_once('=').unwrap();

                    let lhs = expand_simple_ng(state, vars, loc, lhs.trim());
                    let rhs = expand_simple_ng(state, vars, loc, rhs.trim());
                    let var = expand_simple_ng(state, vars, loc, var.trim());

                    if lhs.contains("%") {
                        let (prefix, postfix) = lhs.split_once("%").unwrap();
                        let split = rhs.split_once("%");
                        let min_len = prefix.len() + postfix.len();

                        if let Some(v) = vars.get(var.trim()) {
                            let v = v.clone().eval(state, loc, vars);
                            let mut out = String::new();
                            for v in v.split_whitespace() {
                                if v.len() >= min_len && v.starts_with(prefix) && v.ends_with(postfix) {
                                    if let Some((add_prefix, add_postfix)) = split {
                                        out.extend(add_prefix.chars());
                                        out.extend(v[prefix.len()..v.len() - postfix.len()].chars());
                                        out.extend(add_postfix.chars());
                                    } else {
                                        out.extend(rhs.chars());
                                    }
                                    
                                    out.push(' ');
                                }
                            }
                            out.pop(); // remove last ` `

                            out
                        } else {
                            String::new()
                        }
                    } else if let Some(v) = vars.get(&var) {
                        let v = v.clone().eval(state, loc, vars);
                        let mut out = String::new();
                        for v in v.split_whitespace() {
                            if v.ends_with(&lhs) {
                                out.extend(v[0..v.len() - lhs.len()].chars());
                                out.extend(rhs.chars());
                                out.push(' ');
                            }
                        }
                        out.pop(); // remove last ` `

                        out
                    } else {
                        String::new()
                    }
                }
                SubType::PatSubst => {
                    let args = get_args::<3>(loc, "patsubst", &arg);

                    let lhs = expand_simple_ng(state, vars, loc, args[0].trim());
                    let rhs = expand_simple_ng(state, vars, loc, args[1].trim());
                    let v = expand_simple_ng(state, vars, loc, args[2].trim());

                    if lhs.contains("%") {
                        let (prefix, postfix) = lhs.split_once("%").unwrap();
                        let split = rhs.split_once("%");
                        let min_len = prefix.len() + postfix.len();

                        let mut out = String::new();
                        for v in v.split_whitespace() {
                            if v.len() >= min_len && v.starts_with(prefix) && v.ends_with(postfix) {
                                if let Some((add_prefix, add_postfix)) = split {
                                    out.extend(add_prefix.chars());
                                    out.extend(v[prefix.len()..v.len() - postfix.len()].chars());
                                    out.extend(add_postfix.chars());
                                } else {
                                    out.extend(rhs.chars());
                                }
                                
                                out.push(' ');
                            }
                        }
                        out.pop(); // remove last ` `

                        out
                    } else {
                        let mut out = String::new();
                        for v in v.split_whitespace() {
                            if v == lhs {
                                out.extend(rhs.chars());
                            } else {
                                out.extend(v.chars());
                            }
                            out.push(' ');
                        }

                        out.pop(); // remove last ` `

                        out
                    }
                }
                SubType::Strip => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let mut out = String::new();

                    for a in arg.split_whitespace() {
                        out.extend(a.chars());
                        out.push(' ');
                    }

                    out.pop();

                    out
                }
                SubType::WildCard => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    let mut out = String::new();
                    let options = glob::MatchOptions {
                        case_sensitive: true,
                        require_literal_separator: true,
                        require_literal_leading_dot: true
                    };
                    for entry in glob::glob_with(&arg, options).unwrap() {
                        out.extend(entry.unwrap().to_str().unwrap().chars());
                        out.push(' ');
                    }
                    out.pop();
                    out
                }
                SubType::Value => {
                    let arg = expand_simple_ng(state, vars, loc, &arg);
                    if let Some(v) = vars.get(arg.trim()) {
                        v.value.clone()
                    } else {
                        String::new()
                    }
                }
                _ => todo!(),
            }
        }

        None | Some('$') => '$'.to_string(),

        // these special cases can be handled as variables in
        // the var stack
        //
        // Some('?') => {
        //     let mut out = String::new();
        //     if let Some(rule) = rule {
        //         for p in &rule.prerequisites {
        //             out.extend(p.chars());
        //             out.push(' ');
        //         }
        //         out.pop(); // remove the last pushed ` `
        //     }
        //     out
        // }

        // Some('@') => {
        //     if let Some(rule) = rule {
        //         rule.target.clone()
        //     } else {
        //         String::new()
        //     }
        // }
        Some(v) => {
            if let Some(v) = vars.get(&v.to_string()) {
                v.clone().eval(state, loc, vars).to_string()
            } else {
                String::new()
            }
        }
    }
}

fn expand_simple_ng(
    state: &State,
    vars: &mut HashMap<String, Var>,
    loc: &Location,
    input: &str,
) -> String {
    let mut stack: String = input.chars().rev().collect();
    let mut output = String::new();

    while let Some(c) = stack.pop() {
        match c {
            '$' => {
                output.extend(expand_ng(state, vars, loc, &mut stack).chars());
            }
            // TODO: handle quoting properly
            // '\'' if target_rule.is_none() => {}
            // '"' if target_rule.is_none() => {}
            a => {
                output.push(a);
            }
        }
    }

    output
}

struct Line {
    targets: Option<String>,
}

fn parse_line(state: &mut State, vars: &mut HashMap<String, Var>, location: &Location, src: &str) {
    // Assume we're not gonna be in a rule
    // correct later if we're wrong
    state.in_rule = false;
    let mut chars = src.chars().peekable();

    let mut is_rule = false;
    let mut double_colon = false;

    let mut delim_stack = String::new();

    while match chars.next() {
        Some(')') => {
            delim_stack.pop();
            true
        }
        Some('}') => {
            delim_stack.pop();
            true
        }

        Some('(') => {
            delim_stack.push('(');
            true
        }
        Some('{') => {
            delim_stack.push('{');
            true
        }

        Some(_) if !delim_stack.is_empty() => true,
        
        Some(':') if matches!(chars.peek(), Some('=')) => false,

        Some('=') => false,

        Some(':') if matches!(chars.peek(), Some(':')) => {
            chars.next();
            match chars.peek() {
                Some('=') => false,
                _ => {
                    is_rule = true;
                    double_colon = true;
                    false
                }
            }
        }
        Some(':') => {
            is_rule = true;
            false
        }

        Some(_) => true,
        None => false,
    } {}

    let mut targets = None;
    let mut src = src;
    if is_rule {
        let (t, rhs) = src
            .split_once(if double_colon { "::" } else { ":" })
            .expect("aaaaaaa panic");
        targets = Some(t);
        src = rhs
    }

    if targets.is_none() && src.trim().starts_with("unexport ") {
        for var in expand_simple_ng(state, vars, location, &src.trim()[9..]).split_whitespace() {
            if let Some(var) = vars.get_mut(var) {
                var.unexport();
            }
        }
    } else if targets.is_none() && src.trim().starts_with("unexport") {
        for var in vars.values_mut() {
            // Don't implicitly unexport if explicitly exported
            // TODO: check soundness of exporting and unexporting
            if !var.exported && !matches!(var.origin, Origin::Env) {
                var.unexport();
            }
        }
    } else {
        // FIXME:
        // GNU make handles export X Y=1 as prereqs. we handle it as
        // export the var `X Y` and set it to `1`
        let (export, src) = if src.trim().starts_with("export ") {
            (true, &src.trim()[7..])
        } else if src.trim().starts_with("export") {
            (true, "")
        } else {
            (false, src)
        };

        let (is_var, var_lhs, var_op, var_rhs) = {
            let mut lhs = String::new();
            let mut op = String::new();
            let mut buf = String::new();
            let mut hit_eq = false;
            let mut delim_stack = String::new();
            let mut chars = src.chars();

            while match chars.next() {
                Some(')') => {
                    buf.push(')');
                    delim_stack.pop();
                    true
                }
                Some('}') => {
                    buf.push('}');
                    delim_stack.pop();
                    true
                }

                Some('(') => {
                    buf.push('(');
                    delim_stack.push('(');
                    true
                }
                Some('{') => {
                    buf.push('{');
                    delim_stack.push('{');
                    true
                }

                Some(a) if !delim_stack.is_empty() => {
                    buf.push(a);
                    true
                }

                Some(';') if !hit_eq => {
                    false
                }

                Some('=') if !hit_eq => {
                    hit_eq = true;
                    lhs = buf;
                    buf = String::new();

                    match lhs.pop() {
                        Some(':') => {
                            let x = lhs.pop().expect("better errror message");
                            if x == ':' {
                                op.push(':');
                            } else {
                                lhs.push(x)
                            }
                            op.push(':');
                            op.push('=');
                            true
                        }

                        Some(a) if matches!(a, '?' | '+' | '!') => {
                            op.push(a);
                            op.push('=');
                            true
                        }

                        Some(a) => {
                            lhs.push(a);
                            op.push('=');
                            true
                        }

                        None => todo!("better error message for empty var name")
                    }
                }

                Some(a) => {
                    buf.push(a);
                    true
                }
                None => false
            } {}
            (hit_eq, lhs, op, buf)
        };

        if is_var {
            // let (lhs, rhs, var_op) = {
            //     if let Some((lhs, rhs)) = src.split_once("::=") {
            //         (lhs, rhs, VarOp::Store(true))
            //     } else if let Some((lhs, rhs)) = src.split_once(":=") {
            //         (lhs, rhs, VarOp::Store(true))
            //     } else if let Some((lhs, rhs)) = src.split_once("+=") {
            //         (lhs, rhs, VarOp::Append)
            //     } else if let Some((lhs, rhs)) = src.split_once("!=") {
            //         (lhs, rhs, VarOp::Shell)
            //     } else if let Some((lhs, rhs)) = src.split_once("?=") {
            //         (lhs, rhs, VarOp::StoreIfUndef)
            //     } else {
            //         let (lhs, rhs) = src.split_once('=').expect("aaaaa panic");
            //         (lhs, rhs, VarOp::Store(false))
            //     }
            // };
            //
            let lhs = var_lhs;
            let rhs = var_rhs;

            let var_op = match var_op.as_str() {
                "::=" | ":=" => VarOp::Store(true),
                "=" => VarOp::Store(false),
                "+=" => VarOp::Append,
                "!=" => VarOp::Shell,
                "?=" => VarOp::StoreIfUndef,
                _ => panic!()
            };

            let lhs = expand_simple_ng(state, vars, location, &lhs);
            // we're better than GNU make here and allow `X Y=1`
            match var_op {
                VarOp::Store(expand) => {
                    let lhs = lhs.trim().to_string();
                    let rhs = if expand {
                        expand_simple_ng(state, vars, location, &rhs)
                    } else {
                        rhs.to_string()
                    };
                    let var = vars.get_mut(lhs.trim());

                    if let Some(targets) = targets {
                        let targets = expand_simple_ng(state, vars, location, targets)
                            .split_whitespace()
                            .map(|x| x.to_string())
                            .collect();
                        state.rules.push(Rule {
                            location: location.clone(),
                            targets,
                            data: RuleData::Var(lhs, var_op, rhs),
                        });
                    } else {
                        if let Some(var) = var {
                            var.store(rhs.trim().to_string());
                        } else {
                            vars.insert(
                                lhs.clone(),
                                Var::new(
                                    if expand {
                                        Flavor::Simple
                                    } else {
                                        Flavor::Recursive
                                    },
                                    Origin::File,
                                    Some(location.clone()),
                                    lhs,
                                    rhs.trim().to_string(),
                                    export,
                                ),
                            );
                        }
                    }
                }

                VarOp::StoreIfUndef => {
                    let lhs = lhs.trim().to_string();
                    let rhs = rhs.to_string();
                    let var = vars.get_mut(lhs.trim());

                    if let Some(targets) = targets {
                        let targets = expand_simple_ng(state, vars, location, targets)
                            .split_whitespace()
                            .map(|x| x.to_string())
                            .collect();
                        state.rules.push(Rule {
                            location: location.clone(),
                            targets,
                            data: RuleData::Var(lhs, var_op, rhs),
                        });
                    } else {
                        if var.is_none() {
                            vars.insert(
                                lhs.clone(),
                                Var::new(
                                    Flavor::Recursive,
                                    Origin::File,
                                    Some(location.clone()),
                                    lhs,
                                    rhs.trim().to_string(),
                                    export,
                                ),
                            );
                        }
                    }
                }

                VarOp::Append => {
                    let lhs = lhs.trim().to_string();
                    let flavor = vars.get(lhs.trim()).map(|x| x.flavor);
                    let rhs = if matches!(flavor, Some(Flavor::Recursive)) {
                        expand_simple_ng(state, vars, location, &rhs)
                    } else {
                        rhs.to_string()
                    };
                    let var = vars.get_mut(lhs.trim());

                    if let Some(targets) = targets {
                        let targets = expand_simple_ng(state, vars, location, targets)
                            .split_whitespace()
                            .map(|x| x.to_string())
                            .collect();
                        state.rules.push(Rule {
                            location: location.clone(),
                            targets,
                            data: RuleData::Var(lhs, var_op, rhs),
                        });
                    } else {
                        if let Some(var) = var {
                            var.append(rhs.trim());
                        } else {
                            vars.insert(
                                lhs.clone(),
                                Var::new(
                                    Flavor::Recursive,
                                    Origin::File,
                                    Some(location.clone()),
                                    lhs,
                                    rhs.trim().to_string(),
                                    export,
                                ),
                            );
                        }
                    }
                }

                _ => todo!(),
            }
        } else if let Some(targets) = targets {
            state.in_rule = true;
            // multiple recipies can be handled by shell `;`. this allows for `@cmd; cmd; cmd`
            // to be handled properly
            let (prereqs, recipie) = {
                if let Some((prereqs, recpie)) = src.split_once(';') {
                    (prereqs, Some(recpie))
                } else {
                    (src, None)
                }
            };
            let prereqs = expand_simple_ng(state, vars, location, prereqs);
            // let prereqs = prereqs.trim().split_whitespace().map(|x| { x.to_string(); x.push(' '); x }).collect();
            let targets = expand_simple_ng(state, vars, location, targets)
                .split_whitespace()
                .map(|x| x.to_string())
                .collect::<Vec<_>>();
            state.rules.push(Rule {
                location: location.clone(),
                targets: targets.clone(),
                data: RuleData::Prereq(double_colon, prereqs),
            });
            if let Some(r) = recipie {
                state.rules.push(Rule {
                    location: location.clone(),
                    targets: targets.clone(),
                    data: RuleData::Recipie(r.into()),
                })
            }
        } else if export {
            let mut export_all = true;
            for var in expand_simple_ng(state, vars, location, src).split_whitespace() {
                export_all = false;
                if let Some(var) = vars.get_mut(var) {
                    var.export();
                }
            }
            if export_all {
                for var in vars.values_mut() {
                    // Don't implicitly export if explicitly unexported
                    if !var.unexported {
                        var.export();
                    }
                }
            }
        } else {
            expand_simple_ng(state, vars, location, src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_ng() {
        // let mut state = State::default();
        // vars.insert(
        //     "X".into(),
        //     Var::new(Flavor::Simple, Origin::File, "X".into(), "1".into(), false),
        // );
        // vars.insert(
        //     "Y".into(),
        //     Var::new(Flavor::Simple, Origin::File, "Y".into(), "$X".into(), false),
        // );
        // vars.insert(
        //     "Z".into(),
        //     Var::new(
        //         Flavor::Recursive,
        //         Origin::File,
        //         "Y".into(),
        //         "$X".into(),
        //         false,
        //     ),
        // );

        // let tests = [
        //     ("$X", "1"),
        //     ("${X}", "1"),
        //     ("$(X)", "1"),
        //     ("$Y", "$X"),
        //     ("$Y${Z}$(X)", "$X11"),
        //     ("$Z", "1"),
        //     ("$$", "$"),
        // ];

        // for (src, out) in tests {
        //     eprintln!("testing expansion of `{}` to `{}`", src, out);
        //     assert_eq!(
        //         super::expand_simple_ng(&state,vars, l&Location::default(), None, &src),
        //         out
        //     );
        // }
    }

    #[test]
    fn parse_line_test() {
        let mut state = State::default();
        let mut vars = HashMap::new();

        super::parse_line(&mut state, &Location::default(), "test=1");
        super::parse_line(&mut state, &Location::default(), "test+=1");
        super::parse_line(&mut state, &Location::default(), "x: test+=1");
        super::parse_line(&mut state, &Location::default(), "x: a b");
        eprintln!(
            "{} = {}",
            super::expand_simple_ng(&state, &mut vars, &Location::default(), "$(test)"),
            "1"
        );

        eprintln!("{:#?}", state);
        assert!(false)
    }

    // #[test]
    // fn var_stack() {
    //     let stack = VarStack::new();
    //     stack.push();
    // }
}

// // TODO: var stack

// struct VarStack<'a>(Option<&'a VarStack<'a>>, HashMap<String, Var>);

// impl<'a> VarStack<'a> {
//     pub fn new() -> VarStack<'static> {
//         VarStack(None, HashMap::new())
//     }

//     pub fn push<'b>(&'b self) -> VarStack<'b> {
//         VarStack(Some(self), HashMap::new())
//     }

//     pub fn get(&self, var: &str) -> Option<&Var> {
//         if let Some(var) = self.1.get(var.into()) {
//             Some(var)
//         } else if let Some(prev) = self.0 {
//             prev.get(var)
//         } else {
//             None
//         }
//     }

//     pub fn get_mut(&mut self, var: &str) -> Option<&mut Var> {
//         if let Some(var) = self.1.get_mut(var.into()) {
//             Some(var)
//         } else if let Some(prev) = self.0 {
//             if let Some(v) = prev.get(var) {
//                 self.1.insert(var.into(), v.clone());
//                 self.get_mut(var.into())
//             } else {
//                 None
//             }
//         } else {
//             None
//         }
//     }
// }
