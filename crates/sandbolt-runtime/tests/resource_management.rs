use sandbolt_runtime::EngineLimits;
use sandbolt_runtime::api::{EvalOptions, JsEngine};

fn run(script: &str, result: &str) -> String {
    let mut engine = JsEngine::new(EngineLimits::default()).unwrap();
    engine.eval(script, EvalOptions::default()).unwrap();
    engine.run_jobs().unwrap();
    engine
        .eval_to_string(result, EvalOptions::default())
        .unwrap()
}

#[test]
fn for_head_resources_survive_continue_and_dispose_on_exit() {
    assert_eq!(
        run(
            r#"
        const log = [];
        const resource = name => ({ [Symbol.dispose]() { log.push(name); } });
        function fail() { throw 'initializer'; }
        outer: for (let i = 0; i < 2; i++) {
          let j = 0;
          for (using first = resource('first'), second = resource('second');; j++) {
            log.push(i + '.' + j);
            if (i === 0) continue outer;
            if (j === 0) continue;
            break;
          }
        }
        try {
          for (using third = resource('third'), unused = fail();;) {}
        } catch (error) { log.push(error); }
        "#,
            "log.join(',')"
        ),
        "0.0,second,first,1.0,1.1,second,first,third,initializer"
    );
}

#[test]
fn async_disposal_awaits_an_evaluated_null_binding_only() {
    assert_eq!(
        run(
            r#"
        const log = [];
        async function f(skip) {
          block: {
            if (skip) break block;
            await using resource = null;
          }
          log.push(skip ? 'skipped' : 'awaited');
        }
        f(false);
        f(true);
        log.push('caller');
        "#,
            "log.join(',')"
        ),
        "skipped,caller,awaited"
    );
}

#[test]
fn async_iterator_disposal_uses_intrinsic_promise_reactions() {
    assert_eq!(
        run(
            r#"
        const log = [];
        const proto = Object.getPrototypeOf(Object.getPrototypeOf((async function* () {}).prototype));
        const dispose = proto[Symbol.asyncDispose];
        const then = Promise.prototype.then;
        Promise.prototype.then = function () { throw 'unexpected then'; };
        const iter = {
          return() {
            log.push(this === iter, arguments.length);
            return Promise.resolve('discarded');
          }
        };
        then.call(dispose.call(iter), value => log.push(value === undefined));
        const marker = { get return() { throw 'getter'; } };
        then.call(dispose.call(marker), undefined, error => log.push(error));
        const promise = Promise.resolve();
        Object.defineProperty(promise, 'constructor', { get() { throw 'constructor'; } });
        then.call(dispose.call({ return() { return promise; } }), undefined, error => log.push(error));
        "#,
            "log.join(',')"
        ),
        "true,0,getter,constructor,true"
    );
}

#[test]
fn zip_closes_before_reentering_only_from_suspended_start() {
    assert_eq!(
        run(
            r#"
        const log = [];
        for (const keyed of [false, true]) {
          for (const started of [false, true]) {
            const inner = {
              next() { return { value: 1, done: false }; },
              return() {
                for (const name of ['next', 'return']) {
                  try { log.push(zip[name]().done); }
                  catch (error) { log.push(error.name); }
                }
                return {};
              }
            };
            const zip = keyed ? Iterator.zipKeyed({ inner }) : Iterator.zip([inner]);
            if (started) zip.next();
            log.push(zip.return().done, zip.next().done);
          }
        }
        "#,
            "log.join(',')"
        ),
        "true,true,true,true,TypeError,TypeError,true,true,true,true,true,true,TypeError,TypeError,true,true"
    );
}
