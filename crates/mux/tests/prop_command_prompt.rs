//! Totality of the command-prompt parser. `plexy-glass cmd LINE...` reuses this
//! grammar verbatim on untrusted argv, so `command_prompt::parse` must be total:
//! arbitrary text returns `Ok` or `Err`, never panics. Mirrors `prop_config`'s
//! decoder-totality property (there is no serializer to round-trip against).

use hegel::{TestCase, generators as gs};
use plexy_glass_mux::command_prompt::{VERBS, parse};

/// Arbitrary text (control bytes, huge/empty input, malformed args, multibyte)
/// never panics the parser.
#[hegel::test(test_cases = 1000)]
fn parse_never_panics(tc: TestCase) {
    let line = tc.draw(gs::text());
    tc.note(&format!("line = {line:?}"));
    let _ = parse(&line); // must not panic regardless of Ok/Err
}

/// Verb-arity coverage: join a known verb with random argument tokens the way
/// `plexy-glass cmd` joins its LINE words, so every verb's arg branch (arity
/// checks, number parsing, path/name tails) is fed junk. Still total.
#[hegel::test(test_cases = 1000)]
fn parse_verb_with_random_args_never_panics(tc: TestCase) {
    let vi = tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(VERBS.len() - 1),
    );
    let verb = VERBS[vi];
    let n_args = tc.draw(gs::integers::<usize>().min_value(0).max_value(4));
    let mut line = verb.to_string();
    for _ in 0..n_args {
        line.push(' ');
        line.push_str(&tc.draw(gs::text().max_size(8)));
    }
    tc.note(&format!("line = {line:?}"));
    let _ = parse(&line);
}
