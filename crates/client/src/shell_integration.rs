//! OSC 133 shell-integration snippets emitted by `plexy-glass shell-integration
//! <shell>`. The user adds one line to their shell rc (e.g.
//! `eval "$(plexy-glass shell-integration zsh)"`) and the headline command-block
//! features light up: exit-status borders, prompt navigation, block mode, the
//! history palette's output search, `run`, and completion notifications.
//!
//! `133;A` = prompt start, `133;B` = command start, `133;C` = pre-exec,
//! `133;D;<exit>` = command done. Block detection needs A, C, and D.

const BASH: &str = r#"# plexy-glass OSC 133 shell integration (bash)
__plexy_osc133_pre()  { printf '\033]133;C\007'; }
__plexy_osc133_post() { printf '\033]133;D;%s\007' "$?"; }
__plexy_osc133_ps1()  { PS1="\[\033]133;A\007\]${PS1}\[\033]133;B\007\]"; }
case "$PROMPT_COMMAND" in
  *__plexy_osc133_post*) ;;
  *) PROMPT_COMMAND="__plexy_osc133_post; __plexy_osc133_ps1${PROMPT_COMMAND:+; $PROMPT_COMMAND}" ;;
esac
trap '__plexy_osc133_pre' DEBUG
"#;

const ZSH: &str = r#"# plexy-glass OSC 133 shell integration (zsh)
__plexy_osc133_preexec() { printf '\033]133;C\007'; }
__plexy_osc133_precmd()  { printf '\033]133;D;%s\007' "$?"; }
typeset -ag preexec_functions precmd_functions
preexec_functions+=(__plexy_osc133_preexec)
precmd_functions+=(__plexy_osc133_precmd)
PS1=$'%{\033]133;A\007%}'"$PS1"$'%{\033]133;B\007%}'
"#;

const FISH: &str = r"# plexy-glass OSC 133 shell integration (fish)
function __plexy_osc133_prompt --on-event fish_prompt
    printf '\033]133;A\007'
end
function __plexy_osc133_preexec --on-event fish_preexec
    printf '\033]133;C\007'
end
function __plexy_osc133_postexec --on-event fish_postexec
    printf '\033]133;D;%s\007' $status
end
";

const NU: &str = r"# Nushell has BUILT-IN OSC 133 — no eval needed. Ensure it's on in config.nu
# (it is by default). Do NOT also set the marks via prompt hooks; that fights
# the built-in. See docs/command-blocks.md.
$env.config.shell_integration.osc133 = true
";

/// The eval-able snippet for `shell`, or `None` for an unknown shell. Accepts
/// the common shell names; `nu`/`nushell` print the built-in config line.
pub fn shell_integration_snippet(shell: &str) -> Option<&'static str> {
    match shell {
        "bash" => Some(BASH),
        "zsh" => Some(ZSH),
        "fish" => Some(FISH),
        "nu" | "nushell" => Some(NU),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_emit_the_required_osc133_marks() {
        // bash/zsh/fish emit the raw marks; block detection needs A, C, and D.
        for sh in ["bash", "zsh", "fish"] {
            let s = shell_integration_snippet(sh).expect("known shell");
            assert!(s.contains("133;A"), "{sh} missing prompt-start (133;A)");
            assert!(s.contains("133;C"), "{sh} missing pre-exec (133;C)");
            assert!(s.contains("133;D"), "{sh} missing done (133;D)");
        }
        // nu is built-in: the snippet points at the flag, not raw marks.
        let nu = shell_integration_snippet("nu").expect("nu");
        assert!(nu.contains("osc133"), "nu mentions the built-in flag");
        assert_eq!(shell_integration_snippet("nushell"), shell_integration_snippet("nu"));
        assert!(shell_integration_snippet("powershell").is_none());
    }
}
