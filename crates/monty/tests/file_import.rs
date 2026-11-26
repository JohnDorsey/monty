//! Tests for importing modules from `.py` files: an import the sandbox does
//! not bundle or cache is one `import` call carrying where its file may be; a
//! host with no module of that name answers with the file it found, which runs
//! as the module and is cached for the session.
//!
//! These cover the host protocol: which imports are asked, unusual answers,
//! dumps and REPL feeds. Python semantics, compared with CPython, are in
//! `test_cases/import__files.py`.

use std::time::Duration;

use monty::{Dump, MontyRepl, MontyRun, ReplOsCall, ReplProgress, Session, SessionRef, dump};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, IMPORT_CALL, ImportArgs, MAX_SEARCH_DIRS, ModuleFile, ModuleRead,
    MontyException, MontyObject, MontyUuid, OsFunctionCall, PrintWriter, ResourceLimits, ResourceTracker,
    find_module_file,
};

/// The files a test's host serves, by virtual path.
type Files<'a> = &'a [(&'a str, &'a str)];

/// A REPL whose working directory is `/proj`.
fn repl() -> MontyRepl {
    let mut repl = MontyRepl::new("main.py", ResourceTracker::default(), CompileOptions::default());
    repl.set_cwd("/proj");
    repl
}

/// Feeds `code`, answering imports with a file from `files` and every other OS
/// call as unhandled. The session is dumped and reloaded at each suspension.
/// Returns the session with the result, or the exception's last traceback line.
fn feed(repl: MontyRepl, code: &str, files: Files<'_>) -> (MontyRepl, Result<MontyObject, String>) {
    feed_logged(repl, code, files, &mut Vec::new())
}

/// [`feed`], recording the arguments of every `import` call in `imports`.
fn feed_logged(
    repl: MontyRepl,
    code: &str,
    files: Files<'_>,
    imports: &mut Vec<ImportArgs>,
) -> (MontyRepl, Result<MontyObject, String>) {
    let mut progress = repl.feed_start(code, Vec::<(String, MontyObject)>::new(), PrintWriter::Stdout);
    loop {
        match progress {
            Ok(ReplProgress::Complete { repl, value }) => return (repl, Ok(value)),
            Ok(ReplProgress::OsCall(call)) => {
                let result = match &call.function_call {
                    OsFunctionCall::Import(args) => {
                        imports.push(args.clone());
                        answer_import(files, args)
                    }
                    other => other.no_handler_result(),
                };
                let ReplProgress::OsCall(call) = round_trip(ReplProgress::OsCall(call)) else {
                    unreachable!("a dump restores the same suspension")
                };
                progress = call.resume(result, PrintWriter::Stdout);
            }
            Ok(_) => panic!("unexpected suspension"),
            Err(err) => {
                let line = err.error.to_string().lines().last().unwrap().to_owned();
                return (err.repl, Err(line));
            }
        }
    }
}

/// A host with no host modules: the module's file from `files`, else not-found.
fn answer_import(files: Files<'_>, args: &ImportArgs) -> ExtFunctionResult {
    find_module_file(args, |path| match files.iter().find(|(file, _)| *file == path) {
        Some((_, text)) => ModuleRead::Text((*text).to_owned()),
        None => ModuleRead::Missing,
    })
    .unwrap_or_else(|| ExtFunctionResult::NotFound(IMPORT_CALL.to_owned()))
}

/// Dumps a suspended feed and loads it back, releasing the original.
fn round_trip(progress: ReplProgress) -> ReplProgress {
    let bytes = dump("main.py", None, &[], SessionRef::Suspended(&progress)).unwrap();
    drop(progress.into_repl());
    let Session::Suspended(progress) = Dump::load(&bytes).unwrap().state else {
        panic!("dumped a suspended feed, loaded something else");
    };
    *progress
}

/// Runs `code` in a fresh session and returns its result.
fn eval(code: &str, files: Files<'_>) -> MontyObject {
    feed(repl(), code, files).1.unwrap()
}

/// The `(module, search path)` of each logged `import` call.
fn asked(imports: &[ImportArgs]) -> Vec<(&str, Vec<&str>)> {
    imports
        .iter()
        .map(|args| {
            (
                args.module.as_str(),
                args.search_path.iter().map(String::as_str).collect(),
            )
        })
        .collect()
}

/// Feeds `code` to a fresh session and returns its first `import` call.
fn first_import(code: &str) -> ReplOsCall {
    let progress = repl()
        .feed_start(code, Vec::<(String, MontyObject)>::new(), PrintWriter::Stdout)
        .unwrap();
    let ReplProgress::OsCall(call) = progress else {
        panic!("expected an import");
    };
    assert_eq!(call.function_call.name(), IMPORT_CALL);
    call
}

/// The full traceback of the exception `code` raises, answering its imports from `files`.
fn traceback(code: &str, files: Files<'_>) -> String {
    let mut call = first_import(code);
    loop {
        let OsFunctionCall::Import(args) = &call.function_call else {
            panic!("expected only imports, got {}", call.function_call.name())
        };
        let result = answer_import(files, args);
        match call.resume(result, PrintWriter::Stdout) {
            Ok(ReplProgress::OsCall(next)) => call = next,
            Ok(_) => panic!("expected the import to raise"),
            Err(err) => return err.error.to_string(),
        }
    }
}

/// The last traceback line of the error resuming `call` with `result` raises.
fn resume_err(call: ReplOsCall, result: impl Into<ExtFunctionResult>) -> String {
    let err = call.resume(result, PrintWriter::Stdout).unwrap_err();
    err.error.to_string().lines().last().unwrap().to_owned()
}

/// The answer that the module's file at `path` holds `source`.
fn module_file(path: &str, source: &str) -> ExtFunctionResult {
    ExtFunctionResult::ModuleFile(ModuleFile {
        path: path.to_owned(),
        source: source.to_owned(),
    })
}

const FOO: &str = "x = 41\ndef f():\n    return x + 1\n";

#[test]
fn a_module_runs_once_per_session() {
    let files = &[("/proj/counter.py", "print('loading')\nitems = []\n")];
    let mut imports = Vec::new();
    let (repl, result) = feed_logged(
        repl(),
        "import counter\nimport counter as again\ncounter.items.append(1)\n(counter is again, again.items)",
        files,
        &mut imports,
    );
    assert_eq!(result.unwrap(), eval("(True, [1])", &[]));
    assert_eq!(asked(&imports), [("counter", vec!["/proj"])]);

    // the cache is session state: a later feed binds the same module, asking nothing
    let mut imports = Vec::new();
    let (_, result) = feed_logged(repl, "import counter\ncounter.items", files, &mut imports);
    assert_eq!(result.unwrap(), eval("[1]", &[]));
    assert_eq!(imports, []);
}

#[test]
fn the_cache_survives_an_idle_dump() {
    let files = &[("/proj/foo.py", FOO)];
    let (repl, result) = feed(repl(), "import foo\nfoo.x = 7", files);
    result.unwrap();
    let bytes = dump("main.py", None, &[], SessionRef::Idle(&repl)).unwrap();
    let Session::Idle(repl) = Dump::load(&bytes).unwrap().state else {
        panic!("dumped while idle");
    };
    let mut imports = Vec::new();
    let (_, result) = feed_logged(*repl, "import foo\nfoo.f()", files, &mut imports);
    assert_eq!(result.unwrap(), MontyObject::int(8));
    assert_eq!(imports, []);
}

#[test]
fn a_missing_module_is_one_import_call() {
    let mut imports = Vec::new();
    let (_, result) = feed_logged(repl(), "import tools", &[], &mut imports);
    assert_eq!(result.unwrap_err(), "ModuleNotFoundError: No module named 'tools'");
    assert_eq!(asked(&imports), [("tools", vec!["/proj"])]);
}

/// A host module answers the import call itself, so no file is used.
#[test]
fn a_host_module_shadows_a_file() {
    let call = first_import("import tools\ntools.x");
    let module = MontyObject::module(
        "tools",
        MontyUuid::from_u128(1),
        [(MontyObject::string("x"), MontyObject::string("host"))],
    );
    let ReplProgress::Complete { value, .. } = call.resume(module, PrintWriter::Stdout).unwrap() else {
        panic!("expected the snippet to complete");
    };
    assert_eq!(value, MontyObject::string("host"));
}

#[test]
fn the_cwd_is_searched_when_the_import_runs() {
    let files = &[
        ("/other/foo.py", "where = 'other'\n"),
        ("/proj/foo.py", "where = 'proj'\n"),
    ];
    assert_eq!(eval("import foo\nfoo.where", files), MontyObject::string("proj"));
    let mut repl = repl();
    repl.set_cwd("/other");
    assert_eq!(
        feed(repl, "import foo\nfoo.where", files).1.unwrap(),
        MontyObject::string("other")
    );
}

/// Every submodule of a file package is its own import call, searching the
/// package's `__path__`, whichever statement imports it.
#[test]
fn a_submodule_is_asked_with_the_package_path() {
    let files = &[
        ("/proj/pkg/__init__.py", ""),
        ("/proj/pkg/sub.py", "value = 3\n"),
        ("/proj/pkg/other.py", "value = 4\n"),
        (
            "/proj/far/__init__.py",
            "from collections import namedtuple\n__path__ = namedtuple('SearchPath', 'directory')('elsewhere')\n",
        ),
        ("/proj/elsewhere/mod.py", "value = 5\n"),
    ];
    let mut imports = Vec::new();
    let code = "import pkg.sub\nfrom pkg import other\nimport far.mod\n(pkg.sub.value, other.value, far.mod.value)";
    let (_, result) = feed_logged(repl(), code, files, &mut imports);
    assert_eq!(result.unwrap(), eval("(3, 4, 5)", &[]));
    assert_eq!(
        asked(&imports),
        [
            ("pkg", vec!["/proj"]),
            ("pkg.sub", vec!["/proj/pkg"]),
            ("pkg.other", vec!["/proj/pkg"]),
            ("far", vec!["/proj"]),
            // a relative `__path__` entry is joined to the working directory
            ("far.mod", vec!["/proj/elsewhere"]),
        ]
    );
}

/// A sandbox-set `__path__` sends only the directories the host would search:
/// entries the host skips (non-`str`, NUL bytes) do not count towards the cap.
#[test]
fn a_long_package_path_is_capped() {
    let files = &[(
        "/proj/pkg/__init__.py",
        "__path__ = ['/nul\\0'] * 40 + [1] + [f'/d{i}' for i in range(40)]\n",
    )];
    let mut imports = Vec::new();
    let (_, result) = feed_logged(repl(), "import pkg.sub", files, &mut imports);
    assert_eq!(result.unwrap_err(), "ModuleNotFoundError: No module named 'pkg.sub'");
    let search_path = &imports.last().unwrap().search_path;
    assert_eq!(search_path.len(), MAX_SEARCH_DIRS);
    assert_eq!(search_path[0], "/d0");
    assert_eq!(search_path.last().unwrap(), &format!("/d{}", MAX_SEARCH_DIRS - 1));
}

#[test]
fn a_non_sequence_package_path_sends_no_search_directories() {
    let files = &[("/proj/pkg/__init__.py", "__path__ = 42\n"), ("/proj/pkg/sub.py", "")];
    let mut imports = Vec::new();
    let (_, result) = feed_logged(repl(), "import pkg.sub", files, &mut imports);
    assert_eq!(result.unwrap_err(), "ModuleNotFoundError: No module named 'pkg.sub'");
    assert_eq!(asked(&imports), [("pkg", vec!["/proj"]), ("pkg.sub", vec![])]);
}

#[test]
fn a_missing_submodule_raises_by_statement() {
    let files = &[("/proj/pkg/__init__.py", "")];
    assert_eq!(
        feed(repl(), "import pkg.nope", files).1.unwrap_err(),
        "ModuleNotFoundError: No module named 'pkg.nope'"
    );
    assert_eq!(
        feed(repl(), "from pkg import nope", files).1.unwrap_err(),
        "ImportError: cannot import name 'nope' from 'pkg' (/proj/pkg/__init__.py)"
    );
}

#[test]
fn an_error_in_a_module_shows_its_file_in_the_traceback() {
    let files = &[("/proj/bad.py", "def f():\n    return 1 / 0\n\nf()\n")];
    insta::assert_snapshot!(traceback("import bad", files), @r#"
    Traceback (most recent call last):
      File "<python-input-0>", line 1, in <module>
        import bad
      File "/proj/bad.py", line 4, in <module>
        f()
        ~~~
      File "/proj/bad.py", line 2, in f
        return 1 / 0
               ~~~~~
    ZeroDivisionError: division by zero
    "#);
}

/// Only the `import` a module body's error unwinds through loses its carets:
/// the call that reached the import keeps them, as in CPython.
#[test]
fn an_import_inside_a_function_shows_its_caller() {
    let files = &[("/proj/bad.py", "def f():\n    return 1 / 0\n\nf()\n")];
    insta::assert_snapshot!(traceback("def g():\n    import bad\ng()", files), @r#"
    Traceback (most recent call last):
      File "<python-input-0>", line 3, in <module>
        g()
        ~~~
      File "<python-input-0>", line 2, in g
        import bad
      File "/proj/bad.py", line 4, in <module>
        f()
        ~~~
      File "/proj/bad.py", line 2, in f
        return 1 / 0
               ~~~~~
    ZeroDivisionError: division by zero
    "#);
}

#[test]
fn a_syntax_error_in_a_module_names_its_file() {
    let files = &[("/proj/bad.py", "x = 1\nx = = 2\n")];
    insta::assert_snapshot!(traceback("import bad", files), @r#"
    Traceback (most recent call last):
      File "<python-input-0>", line 1, in <module>
        import bad
      File "/proj/bad.py", line 2
        x = = 2
            ~
    SyntaxError: Expected an expression
    "#);
    // nothing is cached: the next import asks for the file again
    let mut imports = Vec::new();
    let (repl, _) = feed_logged(repl(), "import bad", files, &mut imports);
    let _ = feed_logged(repl, "import bad", files, &mut imports);
    assert_eq!(asked(&imports), [("bad", vec!["/proj"]), ("bad", vec!["/proj"])]);
}

#[test]
fn unusual_answers() {
    let call = first_import("import foo");
    let call_id = call.call_id;
    assert_eq!(
        resume_err(call, ExtFunctionResult::Future(call_id)),
        "RuntimeError: import of 'foo' cannot be answered with a future"
    );
    let unreadable = MontyException::new(ExcType::OSError, Some("[Errno 5] Input/output error".to_owned()));
    assert_eq!(
        resume_err(first_import("import foo"), ExtFunctionResult::Error(unreadable)),
        "OSError: [Errno 5] Input/output error"
    );

    // a module file answers only an import
    let progress = repl()
        .feed_start(
            "import os\nos.getenv('X')",
            Vec::<(String, MontyObject)>::new(),
            PrintWriter::Stdout,
        )
        .unwrap();
    let ReplProgress::OsCall(getenv) = progress else {
        panic!("expected os.getenv to suspend");
    };
    assert_eq!(
        resume_err(getenv, module_file("/proj/x.py", "")),
        "RuntimeError: only an import can be answered with a module file"
    );
}

/// The reply's path decides whether the module is a package.
#[test]
fn a_package_is_a_file_named_init() {
    let call = first_import("import pkg\n(pkg.__file__, pkg.__path__, pkg.__package__)");
    let ReplProgress::Complete { value, .. } = call
        .resume(module_file("/srv/pkg/__init__.py", ""), PrintWriter::Stdout)
        .unwrap()
    else {
        panic!("expected the snippet to complete");
    };
    assert_eq!(value, eval("('/srv/pkg/__init__.py', ['/srv/pkg'], 'pkg')", &[]));
}

#[test]
fn a_run_with_no_host_finds_no_file() {
    let err = MontyRun::new("import foo".to_owned(), "main.py", vec![], CompileOptions::default())
        .unwrap()
        .run(vec![], ResourceTracker::default(), PrintWriter::Stdout)
        .unwrap_err();
    assert_eq!(
        err.to_string().lines().last(),
        Some("ModuleNotFoundError: No module named 'foo'")
    );
}

/// A `ModuleNotFoundError` the host raises is the import's error, unchanged.
#[test]
fn a_host_import_error_is_raised() {
    let missing_dependency = MontyException::new(
        ExcType::ModuleNotFoundError,
        Some("No module named 'dependency'".to_owned()),
    );
    assert_eq!(
        resume_err(
            first_import("import tools"),
            ExtFunctionResult::Error(missing_dependency)
        ),
        "ModuleNotFoundError: No module named 'dependency'"
    );
}

#[test]
fn a_module_repr_quotes_its_path() {
    let files = &[("/it's/foo.py", "")];
    let mut repl = repl();
    repl.set_cwd("/it's");
    assert_eq!(
        feed(repl, "import foo\nrepr(foo)", files).1.unwrap(),
        MontyObject::string("<module 'foo' from \"/it's/foo.py\">")
    );
}

/// A snippet abandoned while a module body is suspended must not leave the
/// unfinished module cached: the next import runs the file again.
#[test]
fn an_abandoned_import_is_not_cached() {
    let files = &[("/proj/foo/__init__.py", "import os\nstage = os.getenv('STAGE')\n")];
    let progress = first_import("import foo")
        .resume(module_file(files[0].0, files[0].1), PrintWriter::Stdout)
        .unwrap();
    let ReplProgress::OsCall(getenv) = progress else {
        panic!("expected the module body to suspend");
    };
    assert_eq!(getenv.function_call.name(), "os.getenv");
    let repl = getenv.into_repl();

    let mut imports = Vec::new();
    let (_, result) = feed_logged(repl, "import foo", files, &mut imports);
    assert_eq!(asked(&imports), [("foo", vec!["/proj"])]);
    // the body runs again, reaching the `os.getenv` this harness does not serve
    assert_eq!(
        result.unwrap_err(),
        "RuntimeError: 'os.getenv' is not supported in this environment"
    );
}

/// However the snippet is abandoned mid-body, the next import reads the file
/// the host serves then rather than binding the partial module.
#[test]
fn an_abandoned_import_reads_the_file_again() {
    let fixed = &[("/proj/helpers.py", "before = 1\nafter = 2\n")];
    let abandoned = [
        suspended_helpers().into_repl(),
        // a suspension restored from a dump abandons through `ReplSnapshot`
        round_trip(ReplProgress::OsCall(suspended_helpers())).into_repl(),
        suspended_helpers()
            .abort(
                MontyException::new(ExcType::RuntimeError, Some("cancelled".to_owned())),
                PrintWriter::Stdout,
            )
            .unwrap_err()
            .repl,
    ];
    for repl in abandoned {
        let mut imports = Vec::new();
        let (_, result) = feed_logged(
            repl,
            "import helpers\n(helpers.before, helpers.after)",
            fixed,
            &mut imports,
        );
        assert_eq!(asked(&imports), [("helpers", vec!["/proj"])]);
        assert_eq!(result.unwrap(), eval("(1, 2)", &[]));
    }
}

/// A star import reads `__all__` in a native loop whose `__getitem__` calls each
/// restart the dispatch loop's checkpoint countdown, so the loop polls the clock.
#[test]
fn a_star_import_of_an_endless_all_times_out() {
    let endless = "class Names:\n    def __getitem__(self, i):\n        return 'x'\n__all__ = Names()\nx = 1\n";
    let limits = ResourceLimits::default().max_feed_duration(Duration::from_millis(50));
    let mut repl = MontyRepl::new("main.py", ResourceTracker::new(limits), CompileOptions::default());
    repl.set_cwd("/proj");
    let (_, result) = feed(repl, "from endless import *", &[("/proj/endless.py", endless)]);
    // The message carries the measured time, so only its fixed start is compared.
    let error = result.unwrap_err();
    assert!(error.starts_with("TimeoutError: feed time limit exceeded: "), "{error}");
}

/// `import helpers`, suspended at a `Path.read_text` between its two assignments.
fn suspended_helpers() -> ReplOsCall {
    let source = "from pathlib import Path\nbefore = 1\nPath('/proj/wait.txt').read_text()\nafter = 2\n";
    let progress = first_import("import helpers")
        .resume(module_file("/proj/helpers.py", source), PrintWriter::Stdout)
        .unwrap();
    let ReplProgress::OsCall(read) = progress else {
        panic!("expected the module body to suspend");
    };
    assert_eq!(read.function_call.name(), "Path.read_text");
    read
}

// ---- the deterministic memory model: an imported file's source is kept for the session ----

/// What each compiled snippet adds whatever it says, and what each byte of its source adds (see `FORK.md`).
const COMPILED_SNIPPET_BYTES: usize = 640;
const COMPILED_SOURCE_BYTE_BYTES: usize = 12;

/// A module whose body compiles and then raises, so the session keeps its source but no module.
const FAILING_BODY: &str = "raise ValueError('x')\n";

/// `import name`, caught when its body raises, in a session that has done it once already (the first call may
/// set up tables of its own). Returns the session and what one more import adds.
fn model_bytes_of_one_more_failing_import(source: &str) -> usize {
    let files = [("/proj/bad.py", source)];
    let attempt = "try:\n    import bad\nexcept (ValueError, SyntaxError):\n    pass";
    let (repl, result) = feed(repl(), attempt, &files);
    result.unwrap();
    let before = repl.model_bytes();
    let (repl, result) = feed(repl, attempt, &files);
    result.unwrap();
    repl.model_bytes() - before
}

#[test]
fn an_imported_file_is_kept_for_the_session_and_costs_by_its_length() {
    // The module is forgotten when its body fails, so the import retries the file; each try keeps the source.
    assert_eq!(
        model_bytes_of_one_more_failing_import(FAILING_BODY),
        COMPILED_SNIPPET_BYTES + FAILING_BODY.len() * COMPILED_SOURCE_BYTE_BYTES
    );
    let padded = format!("{FAILING_BODY}{}\n", "#".repeat(1000));
    assert_eq!(
        model_bytes_of_one_more_failing_import(&padded),
        COMPILED_SNIPPET_BYTES + padded.len() * COMPILED_SOURCE_BYTE_BYTES
    );
}

#[test]
fn a_file_that_does_not_compile_is_not_kept() {
    assert_eq!(model_bytes_of_one_more_failing_import("def (:\n"), 0);
}

#[test]
fn a_file_that_loads_costs_its_length_beside_the_module_it_makes() {
    // Two modules of the same shape, one with a thousand more bytes of comment: the module's own objects cost the
    // same, so the whole difference is the source.
    let session_after = |name: &str, source: &str| {
        let path = format!("/proj/{name}.py");
        let (repl, result) = feed(repl(), &format!("import {name}"), &[(path.as_str(), source)]);
        result.unwrap();
        repl.model_bytes()
    };
    let short = "x = 1\n";
    let long = format!("{short}{}\n", "#".repeat(1000));
    assert_eq!(
        session_after("mb", &long) - session_after("ma", short),
        (long.len() - short.len()) * COMPILED_SOURCE_BYTE_BYTES
    );
}

#[test]
fn the_cost_of_imported_files_is_the_same_after_a_dump_and_restore() {
    let files = [
        ("/proj/helpers.py", "before = 1\nafter = [before] * 50\n"),
        ("/proj/bad.py", FAILING_BODY),
    ];
    let (repl, result) = feed(
        repl(),
        "import helpers\nfor i in range(3):\n    try:\n        import bad\n    except ValueError:\n        pass",
        &files,
    );
    result.unwrap();
    let before = repl.model_bytes();
    let bytes = dump("main.py", None, &[], SessionRef::Idle(&repl)).unwrap();
    let Session::Idle(restored) = Dump::load(&bytes).unwrap().state else {
        panic!("dumped while idle");
    };
    assert_eq!(restored.model_bytes(), before);
}

/// A loop of imports that each keep a source is stopped by `max_model_memory`, near the limit and never before it.
#[test]
fn a_loop_of_failing_imports_hits_the_model_memory_limit() {
    let limit = 200_000;
    let limits = ResourceLimits::default().max_model_memory(limit);
    let mut repl = MontyRepl::new("main.py", ResourceTracker::new(limits), CompileOptions::default());
    repl.set_cwd("/proj");
    let mut imports = Vec::new();
    let (repl, result) = feed_logged(
        repl,
        "for i in range(100000):\n    try:\n        import bad\n    except (ValueError, SyntaxError):\n        pass",
        &[("/proj/bad.py", FAILING_BODY)],
        &mut imports,
    );
    let error = result.unwrap_err();
    assert!(
        error.starts_with("MemoryError: model memory limit exceeded: "),
        "{error}"
    );
    let price = COMPILED_SNIPPET_BYTES + FAILING_BODY.len() * COMPILED_SOURCE_BYTE_BYTES;
    // Never stopped early: the imports made were enough to reach the limit.
    assert!(
        imports.len() * price >= limit - 2_000,
        "stopped early after {}",
        imports.len()
    );
    // And not far late: the schedule's overshoot is a tenth of the limit at most.
    assert!(repl.model_bytes() <= limit + limit / 10, "{}", repl.model_bytes());
}

/// One file that alone is over the limit stops the script when it is imported, not some thousands of steps later:
/// the tracker is told what the import kept, so it measures at once.
#[test]
fn one_large_file_stops_the_script_at_the_import() {
    let limit = 300_000;
    let limits = ResourceLimits::default().max_model_memory(limit);
    let mut repl = MontyRepl::new("main.py", ResourceTracker::new(limits), CompileOptions::default());
    repl.set_cwd("/proj");
    // Forty thousand bytes of comment cost 480,000 with the base: more than the whole limit.
    let large = format!("{}\n{FAILING_BODY}", "#".repeat(40_000));
    let (repl, result) = feed(
        repl,
        "try:\n    import bad\nexcept ValueError:\n    pass\nx = 0\ny = 0\nz = 0",
        &[("/proj/bad.py", &large)],
    );
    let error = result.unwrap_err();
    assert!(
        error.starts_with("MemoryError: model memory limit exceeded: "),
        "{error}"
    );
    assert!(repl.model_bytes() > limit, "{}", repl.model_bytes());
}
