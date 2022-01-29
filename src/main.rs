#![feature(if_let_guard)]

use std::{
    collections::HashMap,
    fs::File,
    io::{prelude::*, BufReader},
    iter::Peekable,
    path::Path,
    process::{Command, Stdio},
};

// Global makefile state
#[derive(Default, Debug)]
struct State {
    fullname: String,
    basename: String,
    dirname: String,
    curdir: String,
    vars: HashMap<String, String>,
    always_make: bool,
    targets_to_make: Vec<String>,
    silent: bool,
    rules: Vec<Rule>,
    in_rule: bool,
    ignore_errors: bool,
    dryrun: bool,
    /// List of phony target names
    phony: Vec<String>,
    processed: Vec<String>,
}

fn main() -> Result<(), u32> {
    let mut args = std::env::args();

    let mut makefile_names = vec![
        "Makefile".to_owned(),
        "makefile".to_owned(),
        "GNUMakefile".to_owned(),
    ];

    let mut state = State::default();

    let mpath: String = args.next().unwrap().trim().into();
    state.basename = Path::new(&mpath)
        .file_name()
        .unwrap()
        .to_owned()
        .into_string()
        .unwrap();

    state.dirname = Path::new(&mpath)
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .into();

    let olddir: String = std::env::current_dir().unwrap().to_str().unwrap().into();
    state.curdir = olddir.clone();

    state.fullname = mpath.clone();
    state.vars.insert("MAKE".into(), mpath);

    for (a, b) in std::env::vars() {
        state.vars.insert(a, b);
    }
    state.vars.insert("SHELL".into(), "/bin/sh".into());

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
        for arg in sargs {
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
                s if s.starts_with("--directory=") => {

                }
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
                    let mut n = args.next().expect("");

                    makefile_names = vec![n]
                },
                "s" | "--silent" | "--quiet" => {
                    state.silent = true;
                    makeflags.push('s');
                }
                "n" | "--just-print" | "--dry-run" | "--recon" => {
                    state.dryrun = true;
                }
                "--no-silent" => {
                    state.silent = false;
                }
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
                        state.vars.insert(l, v);
                    } else {
                        state.targets_to_make.push(l);
                    }
                }
                _ => return Err(1),
            }
        }
    }

    state.vars.insert("MAKEFLAGS".into(), makeflags);

    let makefile = makefile_names
        .into_iter()
        .find(|name| Path::new(&name).exists())
        .expect("No makefiles found")
        .clone();

    let mut leaving = None;

    if !state.silent && dashC {
        println!("{}: Entering directory '{}'", state.basename, state.curdir);
        leaving = Some(format!("{}: Leaving directory '{}'", state.basename, state.curdir));
    }

    let r = state_machine(state, &makefile);

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
fn read_logical_line(file: &mut BufReader<File>, eof: &mut bool, line_no: &mut usize) -> String {
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
            let mut chars = tmp_line.chars().peekable();

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
                    (false, false, 0, '#') => discard = true,
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
                        line.push('\\');
                        line.push(chars.next().unwrap());
                    }
                    (false, false, _, '\\') => match chars.next() {
                        Some('\\') => line.push('\\'),
                        Some('\n') => needs_line = true,
                        _ => {}
                    },
                    (_, _, _, a) => {
                        if !discard {
                            line.push(a);
                        }
                    }
                }
            }
        } else {
            *eof = true;
        }
    }

    line
}

fn process_specials(state: &mut State) {
    for t in &state.rules {
        if let Some(first_target) = t.targets.get(0) {
            match first_target.as_str() {
                ".SILENT" => state.silent = true,
                ".PHONY" => {
                    if let RuleData::Prereq(prereqs) = &t.data {
                        state.phony.extend(prereqs.clone());
                    }
                }
                _ => {}
            }
        }
    }
}

/// setsup some options aswell
fn select_targets(state: &State) -> Vec<String> {
    let mut best_matches = Vec::new();
    for t in &state.rules {
        let first_target = t.targets.get(0).map(|x| x.clone());
        let first_target = first_target.unwrap_or_default();
        match t {
            Rule {
                data: RuleData::Prereq(prereqs),
                ..
            } if first_target == ".DEFAULT" => {
                best_matches = prereqs.clone();
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

fn state_machine(mut state: State, file: &str) -> Result<(), u32> {
    process_lines(&mut state, file);

    process_specials(&mut state);

    let mut targets_to_make = state.targets_to_make.clone();

    if targets_to_make.is_empty() {
        targets_to_make = select_targets(&mut state)
    }

    for t in targets_to_make {
        process_target(&mut state, &t);
    }

    Ok(())
}

enum SubType {
    Var,
    Info,
    Shell,
    Subst,
    Warn
}

/// - `stack` - Output characters
/// - `src` - The src string to read from.
fn expand(state: &mut State, loc: &Location, rule: Option<&TargetRule>, stack: &mut String) {
    // $ must have been consumed already
    match stack.pop() {
        Some(e) if e == '(' || e == '{' => {
            let mut sub_type = SubType::Var;
            let mut var_name = String::new();
            let mut expanded = false;
            let mut in_fun = false;
            while match stack.pop() {
                Some(a) if a == if e == '(' { ')' } else { '}' } => false,
                Some('$') => {
                    expanded = true;
                    expand(state, loc, rule, stack);
                    true
                }
                Some(' ') if !expanded && !in_fun => {
                    in_fun = true;
                    match var_name.as_str() {
                        "info" => {
                            sub_type = SubType::Info;
                            var_name = String::new();
                        }
                        "shell" => {
                            sub_type = SubType::Shell;
                            var_name = String::new();
                        }
                        "subst" => {
                            sub_type = SubType::Subst;
                            var_name = String::new();
                        }
                        "warning" => {
                            sub_type = SubType::Warn;
                            var_name = String::new();
                        }
                        _ => {
                            var_name = String::new();
                        }
                    }
                    true
                }
                Some(a) => {
                    var_name.push(a);
                    true
                }
                None => {
                    panic!("{:#?}", state);
                }
            } {}
            match sub_type {
                SubType::Var => {
                    let s = state
                        .vars
                        .get(&var_name)
                        .map(|x| x.clone())
                        .unwrap_or_default();
                    stack.extend(s.chars().rev());
                }
                SubType::Info => {
                    println!("{}", var_name);
                }
                SubType::Shell => {

                    let cmd = process_for_shell(&var_name);

                    let cmd_name = cmd.split_whitespace().next().unwrap();

                    let cnf_status = Command::new("/bin/sh")
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .arg("-c")
                        .arg(format!("command -V {}", cmd_name))
                        .status()
                        .expect("command failed");
                    if !cnf_status.success() {
                        eprintln!("{}: {}: No such file or directory", state.basename, cmd_name);
                        state.vars.insert(
                            ".SHELLSTATUS".into(),
                            "127".into()
                        );
                    } else {
                        let out = Command::new("sh")
                            .arg("-c")
                            .arg(cmd)
                            .output()
                            .expect("Command failed to execute");
                        let s = String::from_utf8(out.stdout).unwrap();
                        stack.extend(s.chars().rev());

                        state.vars.insert(
                            ".SHELLSTATUS".into(),
                            format!("{}", out.status.code().unwrap_or_default()),
                        );
                    }
                }
                SubType::Subst => {
                    let mut args = var_name.split(",");
                    let from = args.next().unwrap();
                    let to = args.next().unwrap();
                    let text = args.next().unwrap();
                    let out = text.replace(from, to);
                    stack.extend(out.chars().rev());
                }
                SubType::Warn => {
                    eprintln!("{}:{}: {}", loc.file_name, loc.line, var_name);
                }
            }
        }

        Some('?') => {
            if let Some(rule) = rule {
                for p in &rule.prerequisites {
                    stack.extend(p.chars().rev());
                    stack.push(' ');
                }
                stack.pop(); // remove the last pushed ` `
            }
        }

        Some('@') => {
            if let Some(rule) = rule {
                stack.extend(rule.target.chars().rev());
            }
        }

        _ => {}
    }
}

#[derive(Debug, Clone)]
struct Location {
    file_name: String,
    line: usize,
}

#[derive(Debug)]
enum Context {
    Unknown,
    Rule(String, Option<String>, Vec<String>),
    Var(String),
}

fn process_lines(state: &mut State, file_name: &str) {
    let file = File::open(file_name).expect("can't find file");
    let mut file = BufReader::new(file);
    let mut eof = false;

    let mut location = Location {
        file_name: file_name.into(),
        line: 0,
    };

    // TODO: .RECIPIEPREFIX
    let recipie_prefix = '\t';
    while !eof {
        let line = read_logical_line(&mut file, &mut eof, &mut location.line);
        match line {
            l if l.starts_with(recipie_prefix) && state.in_rule => {
                let r = match state.rules.last() {
                    Some(Rule { targets, data: RuleData::Prereq(..), .. }) | Some(Rule { targets, data: RuleData::Recipie(..), .. }) => {
                        Rule {
                            location: location.clone(),
                            targets: targets.clone(),
                            data: RuleData::Recipie(l)
                        }
                    }

                    _ => panic!()
                };
                state.rules.push(r);
            }
            l if l.starts_with(recipie_prefix) && !state.in_rule => {
                panic!("Not currently within a rule");
            }
            l if l.starts_with("include ") => {
                state.in_rule = false;

                process_lines(state, &l[8..].trim());
            }
            l if l.starts_with("-include ") | l.starts_with("sinclude ") => {
                state.in_rule = false;
                if Path::new(l[8..].trim()).exists() {
                    process_lines(state, &l[8..].trim());
                }
            }
            l => {
                let mut stack = l.chars().rev().collect::<String>();
                let mut out = String::new();
                let mut stop_expanding = false;
                let mut context = Context::Unknown;
                while let Some(c) = stack.pop() {
                    match c {
                        '$' if stack.ends_with('$') => {
                            out.push(stack.pop().unwrap());
                            out.push('$');
                        }
                        '$' if !stop_expanding => {
                            expand(state, &location, None, &mut stack);
                        }
                        '=' if !matches!(context, Context::Var(_) | Context::Rule(..)) => {
                            context = Context::Var(out);
                            out = String::new();
                            stop_expanding = true;
                        }
                        ':' if !matches!(context, Context::Var(_)) => match stack.pop() {
                            Some('=') => {
                                context = Context::Var(out);
                                out = String::new();
                            }
                            Some(a) => {
                                context = Context::Rule(out, None, Vec::new());
                                out = String::new();
                                stack.push(a);
                                stop_expanding = true;
                            }
                            None => {
                                context = Context::Rule(out, None, Vec::new());
                                out = String::new();
                                stop_expanding = true;
                            }
                        }
                        ';' if let Context::Rule(a, None, v) = context => {
                            stop_expanding = true;
                            context = Context::Rule(a, Some(out), v);
                            out = String::new();
                        }
                        ';' if let Context::Rule(a, Some(pre), v) = &mut context => {
                            v.push(out);
                            out = String::new();
                        }
                        a => {
                            out.push(a);
                        }
                    }
                }

                match context {
                    Context::Var(v) => {
                        state.in_rule = false;
                        state
                            .vars
                            .insert(v.trim().to_string(), out.trim().to_string());
                    }
                    Context::Rule(targets, Some(prerequisites), recipies) => {
                        let mut recipies = recipies;
                        recipies.push(out);

                        if targets.trim() != ".PHONY" {
                            state.in_rule = true;
                            let targets: Vec<String> = targets
                                .split_ascii_whitespace()
                                .map(|s| s.trim().into())
                                .collect();
                            state.rules.push(Rule {
                                location: location.clone(),
                                targets: targets.clone(),
                                data: RuleData::Prereq(
                                    prerequisites
                                        .split_ascii_whitespace()
                                        .map(|s| s.trim().into())
                                        .collect(),
                                ),
                            });
                            for r in recipies {
                                state.rules.push(Rule {
                                    location: location.clone(),
                                    targets: targets.clone(),
                                    data: RuleData::Recipie(r),
                                })
                            }
                        }
                    }
                    Context::Rule(targets, None, _) => {
                        if targets.trim() != ".PHONY" {
                            state.in_rule = true;
                            let targets: Vec<String> = targets
                                .split_ascii_whitespace()
                                .map(|s| s.trim().into())
                                .collect();
                            state.rules.push(Rule {
                                location: location.clone(),
                                targets: targets,
                                data: RuleData::Prereq(
                                    out.split_ascii_whitespace()
                                        .map(|s| s.trim().into())
                                        .collect(),
                                ),
                            });
                        }
                    }
                    c => {}
                }
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

#[derive(Debug, Clone)]
enum RuleData {
    Prereq(Vec<String>),
    Var(Vec<(String, String)>),
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

fn process_target(state: &mut State, name: &str) {
    if state.processed.contains(&name.to_string()) {
        return;
    } else {
        state.processed.push(name.to_string());
    }

    let mut target_rule = TargetRule::default();
    target_rule.target = name.to_owned();

    let mut recipies = Vec::new();

    let mut was_prereq = false;
    let mut was_recipies = false;
    for rule in &state.rules {
        if rule.targets.contains(&name.to_owned()) {
            match &rule.data {
                RuleData::Var(vars) => {
                    for var in vars {
                        let (a, b) = var;
                        target_rule.vars.insert(a.into(), b.into());
                    }
                    was_prereq = false;
                    was_recipies = false;
                }
                RuleData::Prereq(prereqs) => {
                    target_rule.prerequisites.extend(prereqs.clone());
                    was_prereq = true;
                    was_recipies = false;
                }
                RuleData::Recipie(r) => {
                    if !recipies.is_empty() && !was_recipies {
                        if !was_prereq {
                            panic!();
                        }

                        eprintln!("WARNING: overwriting previous recipies for {}", name);
                        recipies = Vec::new();
                    }
                    was_recipies = true;
                    was_prereq = false;
                    recipies.push((rule.location.clone(), r.clone()));
                }
            }
        }
    }

    for t in &target_rule.prerequisites {
        process_target(state, t);
    }

    let path = Path::new(name);
    let mut needs_updating = false;
    if state.phony.contains(&name.to_string()) {
        needs_updating = true;
    } else if let Ok(Ok(time)) = path.metadata().map(|m| m.modified()) {
        for p in &target_rule.prerequisites {
            if state.phony.contains(p) {
                needs_updating = true;
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

    if needs_updating {
        let mut expanded = Vec::new();
        
        for (loc, r) in &recipies {
            let mut stack: String = r.chars().rev().collect();
            let mut cmd = String::new();

            while let Some(c) = stack.pop() {
                match c {
                    '$' if stack.ends_with('$') => {
                        cmd.push(stack.pop().unwrap());
                    }
                    '$' => {
                        expand(state, &loc, Some(&mut target_rule), &mut stack);
                    }
                    a => {
                        cmd.push(a);
                    }
                }
            }

            let mut cmd = cmd.trim();

            if cmd.trim().is_empty() {
                continue;
            }

            expanded.push((loc.clone(), cmd.to_string()));
        }

        for (loc, cmd) in &expanded {
            let mut cmd = cmd.as_str();
            let ignore_errors = if cmd.starts_with('-') {
                cmd = &cmd[1..];
                true
            } else {
                // TODO: state.ignore errors
                state.ignore_errors
            };

            let mut silent = false;

            if cmd.starts_with('@') {
                cmd = &cmd[1..];
                silent = true;
            }

            if (!silent || state.dryrun) && !state.silent {
                println!("{}", cmd);
            }

            let cmd_name = cmd.trim().split_ascii_whitespace().next().unwrap();
            let cnf_status = Command::new("/bin/sh")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .arg("-c")
                .arg(format!("command -V {}", cmd_name))
                .status()
                .expect("command failed");
            if !cnf_status.success() {
                eprintln!("{}: {}: No such file or directory", state.basename, cmd_name);
                
                if ignore_errors {
                    eprintln!(
                        "{}: [{}:{}: {}] Error 127 (ignored)",
                        state.basename,
                        loc.file_name,
                        loc.line,
                        name,
                    );
                } else {
                    eprintln!(
                        "{}: *** [{}:{}: {}] Error 127",
                        state.basename,
                        loc.file_name,
                        loc.line,
                        name,
                    );
                    std::process::exit(2);
                }
                continue;
            }

            let mut leaving = None;

            if !silent && cmd_name == state.fullname {
                println!("{}[1]: Entering directory '{}'", state.basename, state.curdir);
                leaving = Some(format!("{}[1]: Leaving directory '{}'", state.basename, state.curdir));
            } else {
            }

            

            let status = Command::new("/bin/sh")
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .arg("-c")
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
                    std::process::exit(2);
                }
            } else if let Some(s) = leaving {
                println!("{}", s);
            }
        }
    }
}
