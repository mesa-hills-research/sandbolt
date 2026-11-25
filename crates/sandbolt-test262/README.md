# sandbolt-test262

Run the pinned Test262 suite or QuickJS-ng's local tests with one Rust runtime per execution. Tests run in parallel and keep a stable report order.

```sh
cargo run --release -p sandbolt-test262 --features integrated-runtime -- \
  -c ../quickjs-ng/test262.conf -a --diff-errors --summary-by-dir
```

Use `-c ../quickjs-ng/tests.conf` for local tests. Use `-f language/module-code --limit 50` to run a subset. The default deadline is 10 seconds per execution. Run with `--help` for the other options.

The upstream score matches negative tests by exception text, without checking where the exception occurred. The phase-strict score checks both the exception type and its phase. Use `--phase-strict` to select phase-strict failure details and directory counts. `--diff-errors` always compares the upstream score to `errorfile` in the config. `--fail-on-regression` makes new or changed errors exit with status 1. A filtered or limited `--update-errors` leaves untested rows in place.

Local tests that rely on `qjs:os`, `qjs:bjson` or the full `qjs:std` library require those host modules. The runner provides the Test262 host and filesystem module fixtures.

Run the config, baseline and discovery checks against a QuickJS-ng checkout:

```sh
QUICKJS_REF=/path/to/quickjs-ng cargo test -p sandbolt-test262 -- --include-ignored
```
