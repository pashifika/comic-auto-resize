//! Shell-completion scripts, generated from the shared command graph and then guarded.
//!
//! Generated rather than written. A hand-written script is a second copy of the flag list,
//! free to drift from the first; produced from the same `clap::Command` the parser and
//! `--help` are produced from, a flag cannot appear in completion without existing.
//!
//! Guarded because generating is not sufficient. `clap_complete` models neither
//! `require_equals` nor an optional value, and its PowerShell generator emits no possible
//! values at all, so its unmodified output offers this surface completions the parser
//! refuses. Each guard below closes one measured gap and says which; none is inherited on
//! the assumption that another project's reason applies here.
//!
//! Nothing here opens the input or reads filesystem state. A completion script is produced
//! while a shell starts, where a failure is a broken prompt rather than a failed command.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, Write};

use clap::{Arg, Command, ValueEnum, ValueHint};
use clap_complete::aot::{Bash, Fish, PowerShell, Zsh, generate};

/// The shells the two release targets use: bash, zsh and fish for `aarch64-apple-darwin`,
/// PowerShell for `x86_64-pc-windows-msvc`.
///
/// Four rather than `clap_complete::Shell`'s five: elvish ships with neither release target,
/// and CI proves a script in the shell it targets, so advertising a fifth would advertise
/// something unproven. `clap` refuses an unrecognised name against this list, which is what
/// makes the refusal name the shells that exist.
///
/// The variants carry no doc comments on purpose. `clap` turns a variant's doc into per-value
/// help, which gives the option a long-help form, which switches `--help` from the two-column
/// layout to the vertical one — for *every* option on the surface. Naming which host ships
/// which shell is not worth reformatting the whole help to say.
// `PowerShell` ends with the enum's name, which the lint reads as a stutter. Here it is the
// shell's own name: spelling it `Power` to satisfy the lint would rename a product. `expect`
// rather than `allow`, so dropping the variant is told the exemption is no longer needed.
#[expect(
    clippy::enum_variant_names,
    reason = "PowerShell is what the shell is called"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Shell {
    Bash,
    Zsh,
    Fish,
    // Without this the derive's kebab-case rename would spell the variant `power-shell`,
    // which is not what the shell is called.
    #[value(name = "powershell")]
    PowerShell,
}

/// Renders `shell`'s script for the shared command graph.
///
/// Into memory rather than straight to the destination: `clap_complete`'s generators panic on
/// a write error, so handing them standard output would turn a reader that closed the pipe
/// into an abort. Writing to a `Vec` cannot fail, which leaves exactly one fallible write —
/// the one in [`write`], which gets to decide what a closed pipe means. The guards need the
/// whole script in hand anyway.
fn script(shell: Shell) -> Vec<u8> {
    let mut command = crate::command();
    // The name the shell will complete for, taken from the graph rather than from `argv[0]`:
    // a script registers against a command name, and the name a package manager installs is
    // this one whatever the file on the developer's machine happens to be called.
    let name = command.get_name().to_string();
    command.build();
    let mut script = Vec::new();
    match shell {
        Shell::Bash => {
            generate(Bash, &mut command, name, &mut script);
            script = guarded_bash(&script, &command);
        }
        Shell::Zsh => {
            generate(Zsh, &mut command, name, &mut script);
            script = guarded_zsh(&script, &command);
        }
        Shell::Fish => {
            generate(Fish, &mut command, name, &mut script);
            script = guarded_fish(&script, &command);
        }
        Shell::PowerShell => {
            generate(PowerShell, &mut command, name, &mut script);
            script = guarded_powershell(&script, &command);
        }
    }
    script
}

// ---------------------------------------------------------------- the attached-value guards

/// Options whose value may only be attached with `=`, sorted.
///
/// `--progressive` and `--optimizer` are both: they take an optional value and `require_equals`
/// so that a value taken from the next argument cannot swallow the positional input path.
/// `clap_complete` models neither property, so every generator it has offers those values in
/// the *next word* — where the parser will read them as the input path instead. Measured:
/// `comic-auto-resize --progressive false` exits 1 with `false: No such file or directory`.
///
/// Each shell's guard below removes that suggestion and leaves the `=` spelling working.
fn equals_only_options(command: &Command) -> Vec<(String, BTreeSet<String>)> {
    let mut options = Vec::new();
    for argument in command.get_opts().filter(|arg| !arg.is_hide_set()) {
        if !argument.is_require_equals_set() {
            continue;
        }
        let values = possible_values(argument);
        if values.is_empty() {
            continue;
        }
        for name in option_names(argument) {
            options.push((name, values.clone()));
        }
    }
    options.sort();
    options
}

/// Options whose value is a path, sorted.
///
/// `clap`'s question, not this module's: `Arg::get_value_hint` infers `AnyPath` from a
/// `PathBuf` value parser, so `-o/--out` answers and `--ratio`, `--quality`, `--jobs`,
/// `--charset` and `--pwd` do not. Reading "has no possible values" as "takes a path" was a
/// review finding — it made `--ratio=sr` complete to `--ratio=src`, which the parser refuses.
fn path_options(command: &Command) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for argument in command.get_opts().filter(|arg| !arg.is_hide_set()) {
        if matches!(
            argument.get_value_hint(),
            ValueHint::AnyPath | ValueHint::FilePath | ValueHint::DirPath
        ) {
            names.extend(option_names(argument));
        }
    }
    names
}

/// Repairs `clap_complete`'s bash output in three places, each one measured.
///
/// - **Value arms** stop offering their values in the word after an attached-value option,
///   and stop letting `-o default` answer a value prefix nothing matched.
/// - **Path arms** hand the position back to readline instead of `compgen -f`.
/// - **A prelude** stops at `--` and completes the attached spelling on a bash that does not
///   split on `=`.
fn guarded_bash(generated: &[u8], command: &Command) -> Vec<u8> {
    let mut script = utf8(generated).to_owned();
    let attached = bash_value_arms(&mut script, command);
    bash_path_arms(&mut script, command);
    let block = bash_prelude(&attached);
    let anchor = "            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then\n";
    replace_once(
        &script,
        anchor,
        &format!("{block}{anchor}"),
        "bash option-name filter",
        "the root command",
    )
    .into_bytes()
}

/// Rewrites every arm that offers a value set, and returns the attached-value options.
///
/// The two bashes disagree about `=`, and that is the difficulty. bash 5 has it in
/// `COMP_WORDBREAKS`, so `--progressive ` and `--progressive=` both reach the generated `prev`
/// arm and `${COMP_WORDS[COMP_CWORD]}` — `=` for the attached form — tells them apart. Under a
/// space the arm returns an **empty** `COMPREPLY` rather than `compgen -f`: the script
/// registers `-o bashdefault -o default`, so an empty reply hands the position to readline's
/// own filename completion, which keeps a directory's trailing `/` and does not split a name
/// on its spaces. `compgen -f` did both, and every archive in this project's corpus has
/// spaces in its name.
fn bash_value_arms(script: &mut String, command: &Command) -> Vec<(String, String)> {
    let equals_only: BTreeSet<String> = equals_only_options(command)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut attached = Vec::new();
    for argument in command.get_opts().filter(|arg| !arg.is_hide_set()) {
        let values = possible_values(argument);
        if values.is_empty() {
            continue;
        }
        for option in option_names(argument) {
            // The emitted `COMPREPLY=` line is reused verbatim rather than rebuilt: it
            // carries upstream's value order and quoting, and rebuilding it was how the first
            // attempt at this guard broke — the graph's values are sorted, the emitted ones
            // are not.
            let header = format!("\n                {option})\n");
            let start = script.find(&header).unwrap_or_else(|| {
                panic!("clap_complete 4.6.9 no longer emits a bash value arm for `{option}`")
            }) + header.len();
            let line = script[start..]
                .lines()
                .next()
                .filter(|line| line.trim_start().starts_with("COMPREPLY="))
                .unwrap_or_else(|| {
                    panic!(
                        "clap_complete 4.6.9's arm for `{option}` no longer opens with COMPREPLY"
                    )
                })
                .to_owned();
            let guarded = if equals_only.contains(&option) {
                attached.push((
                    option.clone(),
                    values.iter().cloned().collect::<Vec<_>>().join(" "),
                ));
                let nested = bash_no_match_guard("                        ");
                format!(
                    "                    if [[ \"${{COMP_WORDS[COMP_CWORD]}}\" == \"=\" ]]; then\n\
                     \x20   {line}\n\
                     {nested}\n\
                     \x20                   else\n\
                     \x20                       COMPREPLY=()\n\
                     \x20                   fi"
                )
            } else {
                format!("{line}\n{}", bash_no_match_guard("                    "))
            };
            script.replace_range(start..start + line.len(), &guarded);
        }
    }
    attached
}

/// Declines readline's filename fallback when no value matched.
///
/// A prefix no value matches leaves the reply empty and `-o default` then fills it with
/// filenames: `--dct pa` completed to `--dct page`, which the parser refuses. `compopt` says
/// so directly but arrived in bash 4, so where it is missing the reply is the word the user
/// already typed — a completion to itself, which is the old idiom for "nothing to offer" and
/// leaves a trailing space where `compopt` leaves none.
fn bash_no_match_guard(indent: &str) -> String {
    format!(
        "{indent}if [[ ${{#COMPREPLY[@]}} -eq 0 ]]; then\n\
         {indent}    if [[ \"${{BASH_VERSINFO[0]}}\" -ge 4 ]]; then\n\
         {indent}        compopt +o default +o bashdefault\n\
         {indent}    else\n\
         {indent}        COMPREPLY=(\"${{cur}}\")\n\
         {indent}    fi\n\
         {indent}fi"
    )
}

/// Hands each path option's arm back to readline.
///
/// `compgen -f` splits a candidate on its spaces and drops a directory's trailing `/`, so
/// `--out a<Tab>` against `a b.zip` offered `a` and `b.zip` as two names. zsh's `_files` and
/// fish's native completion both get this right unaided; only bash's generated arm does not.
fn bash_path_arms(script: &mut String, command: &Command) {
    for option in path_options(command) {
        let arm = format!(
            "\n                {option})\n                    COMPREPLY=($(compgen -f \"${{cur}}\"))\n"
        );
        let replacement =
            format!("\n                {option})\n                    COMPREPLY=()\n");
        *script = replace_once(script, &arm, &replacement, "bash path arm", &option);
    }
}

/// The block spliced ahead of the generated option-name filter.
fn bash_prelude(attached: &[(String, String)]) -> String {
    let mut block = String::from(
        "            # Everything after `--` is the input path, so no option name belongs here.\n\
         \x20           # An empty reply hands the position to readline's own file completion.\n\
         \x20           for ((_car_i = 1; _car_i < COMP_CWORD; _car_i++)); do\n\
         \x20               if [[ \"${COMP_WORDS[_car_i]}\" == \"--\" ]]; then\n\
         \x20                   COMPREPLY=()\n\
         \x20                   return 0\n\
         \x20               fi\n\
         \x20           done\n",
    );
    if !attached.is_empty() {
        block.push_str(
            "            # An attached value that reached `cur` whole, which is what happens\n\
             \x20           # wherever `=` is not a word break. `$2` is the text readline will\n\
             \x20           # replace: when it is the whole word the candidate has to carry the\n\
             \x20           # option back, and when it is only the value it must not.\n\
             \x20           case \"${cur}\" in\n",
        );
        for (option, values) in attached {
            write!(
                block,
                "                {option}=*)\n\
                 \x20                   if [[ \"$2\" == \"${{cur}}\" ]]; then\n\
                 \x20                       COMPREPLY=($(compgen -P \"${{cur%%=*}}=\" -W \"{values}\" -- \"${{cur#*=}}\"))\n\
                 \x20                   else\n\
                 \x20                       COMPREPLY=($(compgen -W \"{values}\" -- \"${{cur#*=}}\"))\n\
                 \x20                   fi\n\
                 \x20                   return 0\n\
                 \x20                   ;;\n"
            )
            .expect("writing to a String cannot fail");
        }
        block.push_str("            esac\n");
    }
    block
}

/// Turns each attached-value option's `_arguments` spec from `=` into `=-`.
///
/// Two characters, and they are `_arguments`' own way of saying it: `--opt=` accepts the value
/// in the same word or the next one, `--opt=-` accepts it only in the same word. zsh then
/// offers the values after `=` and the input path after a space.
fn guarded_zsh(generated: &[u8], command: &Command) -> Vec<u8> {
    let mut script = utf8(generated).to_owned();
    for (option, _) in equals_only_options(command) {
        // Long options only: zsh spells a short option's attached value differently, and this
        // surface gives neither switch a short form.
        if !option.starts_with("--") {
            continue;
        }
        let spec = format!("'{option}=[");
        let replacement = format!("'{option}=-[");
        script = replace_once(&script, &spec, &replacement, "zsh option spec", &option);
    }
    script.into_bytes()
}

/// Drops `-r` from each attached-value option's `complete` line.
///
/// `-r` is "this option requires a value", which is what makes fish offer the values in the
/// next word. Without it the values are still offered after `=`, because fish splits an
/// attached value itself, and the next word falls through to the input path.
fn guarded_fish(generated: &[u8], command: &Command) -> Vec<u8> {
    let name = command.get_name();
    let mut script = utf8(generated).to_owned();
    for (option, _) in equals_only_options(command) {
        let Some(long) = option.strip_prefix("--") else {
            continue;
        };
        let line = format!("complete -c {name} -l {long} ");
        let start = script
            .find(&line)
            .unwrap_or_else(|| panic!("clap_complete 4.6.9 declares `{option}` to fish"));
        let end = script[start..]
            .find('\n')
            .map_or(script.len(), |offset| start + offset);
        let declared = script[start..end].to_owned();
        let guarded = replace_once(&declared, " -r -f -a ", " -f -a ", "fish arity", &option);
        script.replace_range(start..end, &guarded);
    }
    script.into_bytes()
}

// -------------------------------------------------------------------- the PowerShell guard

/// Where the guard is spliced into `clap_complete`'s PowerShell output: after the generated
/// preamble has resolved `$command` and before it dispatches on it.
const POWERSHELL_ANCHOR: &str = "    $completions = @(switch ($command) {";

/// Everything `clap_complete` 4.6.9's PowerShell generator gets wrong for this surface.
///
/// Measured rather than inherited, and measured twice: the first pass found only the missing
/// possible values, and an independent review found four more by driving
/// `[CommandCompletion]::CompleteInput` against the shipped script. What the stock output
/// does correctly is option *names* with their help text, and that is what it is left to do.
/// What it gets wrong here, each closed below:
///
/// - **No `PossibleValue` at all**, so `--dct <Tab>` offered the option list back, `--dct`
///   among it.
/// - **A bareword extends `$command`**, so `comic-auto-resize book.zip --r` produced
///   `comic-auto-resize;book.zip`, matched no arm, and offered nothing. Writing the input
///   first is the ordinary way to type this command.
/// - **Quoted tokens are compared raw**, so `--dct "if` offered nothing.
/// - **A path option is answered by the option list.** `--out <Tab>` returned every option
///   including `--out`, because the stock body filters names by an empty prefix and so never
///   lets PowerShell's own file completion answer. `--out=sr` returned nothing, because that
///   fallback does not strip `--out=`.
/// - **A known value set with no match falls through to files**, so `--dct sr` completed to
///   `./src`, which the parser then refuses.
///
/// `skillmount` replaces this generator outright, several hundred lines. This does not,
/// because the largest part of that replacement handles `ValueHint::DirPath` and
/// `ValueHint::ExecutablePath` and this surface has neither — its positional is an archive
/// *or* a directory and `--out` is a location *or* a filename, both ordinary file completion.
fn guarded_powershell(generated: &[u8], command: &Command) -> Vec<u8> {
    let generated = utf8(generated);
    // A missing anchor means the pinned generator changed shape, which cannot happen without
    // an edit to `Cargo.toml`: the version is exact. The contract tests generate this script,
    // so a bump that moved the anchor would fail in CI rather than in someone's prompt.
    let split = generated
        .find(POWERSHELL_ANCHOR)
        .expect("clap_complete 4.6.9's PowerShell body dispatches on $command");

    let mut guarded = String::with_capacity(generated.len() * 2);
    guarded.push_str(&generated[..split]);
    powershell_tables(&mut guarded, command, command.get_name());
    guarded.push_str(POWERSHELL_GUARD);
    guarded.push_str(&generated[split..]);
    guarded.into_bytes()
}

/// The facts about the surface the guard dispatches on, emitted as PowerShell data.
///
/// Data rather than generated control flow, so the logic below is one readable block that
/// does not grow with the flag list.
fn powershell_tables(out: &mut String, command: &Command, name: &str) {
    out.push_str("    # What this surface is, emitted from the command graph.\n");
    out.push_str("    $carName = ");
    write_powershell_literal(out, name);
    out.push('\n');

    out.push_str("    $carValues = @{\n");
    for argument in command.get_opts().filter(|arg| !arg.is_hide_set()) {
        let values = possible_values(argument);
        if values.is_empty() {
            continue;
        }
        for option in option_names(argument) {
            out.push_str("        ");
            write_powershell_literal(out, &option);
            out.push_str(" = @(");
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                write_powershell_literal(out, value);
            }
            out.push_str(")\n");
        }
    }
    out.push_str("    }\n");

    write_powershell_array(out, "$carPathOptions", &path_options(command));
    write_powershell_array(
        out,
        "$carEqualsOnly",
        &equals_only_options(command)
            .into_iter()
            .map(|(name, _)| name)
            .collect(),
    );
}

/// The dispatch itself, which is the same for any surface the tables above can describe.
const POWERSHELL_GUARD: &str = r#"
    # This surface has no subcommands, so the generated preamble's bareword walk can only be
    # wrong: `comic-auto-resize book.zip --r` makes $command 'comic-auto-resize;book.zip',
    # which matches no arm below and loses option-name completion entirely. Pinned to the
    # registered name rather than to the typed one, because `./target/release/comic-auto-resize`
    # is the same command and matches no arm either.
    $command = $carName

    # Tokens before the cursor, with quoting resolved. `.Value` is the parsed string of a
    # StringConstantExpressionAst, so a quoted `"--dct"` reads as an option rather than as a
    # word starting with a quote. Never evaluate an element to find its value.
    $carTokens = @($commandAst.CommandElements |
        Where-Object { $_.Extent.EndOffset -lt $cursorPosition } |
        ForEach-Object {
            if ($_ -is [System.Management.Automation.Language.StringConstantExpressionAst]) {
                $_.Value
            } else {
                $_.Extent.Text
            }
        })

    $carOption = $null
    $carPrefix = $wordToComplete
    $carAttached = ''
    # Everything after `--` is the positional input, however it is spelled.
    $carTerminated = $carTokens -contains '--'
    # A bare attached-value option does not consume the next word, so that word is the input
    # path -- but it does not end option parsing either, so an option name may still follow.
    $carPositional = $false
    if (-not $carTerminated) {
        if ($wordToComplete -match '^(--?[^=]+)=(.*)$') {
            $carOption = $Matches[1]
            $carPrefix = $Matches[2]
            $carAttached = "$($Matches[1])="
        } elseif ($carTokens.Count -gt 1 -and $carTokens[-1].StartsWith('-')) {
            if ($carEqualsOnly -notcontains $carTokens[-1]) {
                $carOption = $carTokens[-1]
            } elseif (-not $wordToComplete.StartsWith('-')) {
                $carPositional = $true
            }
        }
    }
    # An unclosed quote is part of the word the shell is completing, not part of the value.
    if ($carPrefix.Length -gt 0 -and ($carPrefix[0] -eq '"' -or $carPrefix[0] -eq "'")) {
        $carQuote = $carPrefix[0]
        $carPrefix = $carPrefix.Substring(1)
        if ($carPrefix.EndsWith($carQuote)) {
            $carPrefix = $carPrefix.Substring(0, $carPrefix.Length - 1)
        }
    }

    if ($null -ne $carOption -and $carValues.ContainsKey($carOption)) {
        $carMatches = @(@($carValues[$carOption]) |
            Where-Object { $_.StartsWith($carPrefix, [System.StringComparison]::OrdinalIgnoreCase) } |
            Sort-Object)
        if ($carMatches.Count -eq 0) {
            # Returning nothing is not the same as suppressing completion: the engine runs its
            # own file completion on an empty result, which is how `--dct sr` completed to
            # `./src` and produced an argument the parser refuses. Handing back the word the
            # user already typed is what makes a no-match a no-op.
            if ($wordToComplete.Length -gt 0) {
                [CompletionResult]::new($wordToComplete, $wordToComplete, [CompletionResultType]::Text, $wordToComplete)
            }
            return
        }
        $carMatches | ForEach-Object {
            [CompletionResult]::new("$carAttached$_", $_, [CompletionResultType]::ParameterValue, $_)
        }
        return
    }

    if ($carTerminated -or $carPositional -or
        ($null -ne $carOption -and $carPathOptions -contains $carOption)) {
        # A path position. Returning nothing lets PowerShell's own file completion answer,
        # which is what should have happened before the stock body offered the option list --
        # but it cannot see past an attached `--out=`, so that spelling is completed here.
        if ($carAttached -eq '') {
            return
        }
        $carCut = [Math]::Max($carPrefix.LastIndexOf('/'), $carPrefix.LastIndexOf('\'))
        if ($carCut -ge 0) {
            $carParent = $carPrefix.Substring(0, $carCut + 1)
            $carLeaf = $carPrefix.Substring($carCut + 1)
        } else {
            $carParent = ''
            $carLeaf = $carPrefix
        }
        $carSearch = if ($carParent -eq '') { '.' } else { $carParent }
        Get-ChildItem -LiteralPath $carSearch -Force -ErrorAction SilentlyContinue |
            Where-Object { $_.Name.StartsWith($carLeaf, [System.StringComparison]::OrdinalIgnoreCase) } |
            Sort-Object -Property Name |
            ForEach-Object {
                # Quoted as one argument. A filename is filesystem data, and this project's own
                # corpus has names with spaces, brackets and parentheses: emitted raw, PowerShell
                # read `(一般コミック)` as a subexpression and tried to run it.
                $carText = "$carAttached$carParent$($_.Name)"
                if ($carText -notmatch '^[A-Za-z0-9_./\\:=+-]+$') {
                    $carText = "'" + $carText.Replace("'", "''") + "'"
                }
                [CompletionResult]::new($carText, $_.Name, [CompletionResultType]::ParameterValue, $carText)
            }
        return
    }

"#;

// -------------------------------------------------------------------------------- helpers

/// The generated script as text. `clap_complete` builds it from `String`s, so this cannot
/// fail for any input the graph can produce.
fn utf8(generated: &[u8]) -> &str {
    std::str::from_utf8(generated).expect("clap_complete emits UTF-8")
}

/// Replaces `needle` once, failing loudly when the pinned generator no longer emits it.
///
/// A guard that silently found nothing is a guard that stopped guarding, and the defect it
/// was written for would come back with no test failing. The dependency is pinned exactly, so
/// this can only fire on a version bump — and the contract tests generate every script, so it
/// fires in CI rather than in someone's prompt.
fn replace_once(text: &str, needle: &str, replacement: &str, what: &str, option: &str) -> String {
    assert!(
        text.contains(needle),
        "clap_complete 4.6.9 no longer emits the {what} this guard rewrites for `{option}`"
    );
    text.replacen(needle, replacement, 1)
}

/// Every spelling an option answers to, long and short.
fn option_names(argument: &Arg) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(shorts) = argument.get_short_and_visible_aliases() {
        names.extend(shorts.into_iter().map(|short| format!("-{short}")));
    }
    if let Some(longs) = argument.get_long_and_visible_aliases() {
        names.extend(longs.into_iter().map(|long| format!("--{long}")));
    }
    names
}

/// An argument's visible possible values, sorted so the script is byte-stable.
fn possible_values(argument: &Arg) -> BTreeSet<String> {
    argument
        .get_possible_values()
        .into_iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_owned())
        .collect()
}

/// `    $name = @('a', 'b')`.
fn write_powershell_array(out: &mut String, name: &str, values: &BTreeSet<String>) {
    out.push_str("    ");
    out.push_str(name);
    out.push_str(" = @(");
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        write_powershell_literal(out, value);
    }
    out.push_str(")\n");
}

/// A single-quoted PowerShell literal, where a quote is escaped by doubling it.
///
/// Correct for the ASCII option names and possible values this surface has, which is all that
/// reaches it: every argument is `@(…)` from the compiled command graph, and nothing from the
/// command line, the environment or the filesystem gets here. It is **not** a general escaper
/// — PowerShell also terminates a single-quoted string on U+2018 and U+2019, which this does
/// not double — so a surface that ever took an option name or possible value from outside the
/// binary would need more than this.
fn write_powershell_literal(out: &mut String, value: &str) {
    out.push('\'');
    let mut parts = value.split('\'');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for part in parts {
        out.push_str("''");
        out.push_str(part);
    }
    out.push('\'');
}

/// Writes `shell`'s script, treating a reader that closed the pipe as success.
///
/// `comic-auto-resize --completions bash | head` is an ordinary thing for a person to do, and
/// the shell that pipes a script into `source` at startup is doing the same shape of thing.
/// Every other write failure is reported: a script truncated by a full disk is not one a
/// shell should source.
///
/// # Errors
///
/// Any write or flush failure other than a closed pipe.
pub(crate) fn write(shell: Shell, writer: &mut dyn Write) -> io::Result<()> {
    let script = script(shell);
    match writer.write_all(&script).and_then(|()| writer.flush()) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::{Shell, write};
    use std::io::{self, Write};

    /// A destination that fails every write with one chosen error.
    struct Failing(io::ErrorKind);

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.0, "test destination"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(self.0, "test destination"))
        }
    }

    /// A reader that closed the pipe is not a failure.
    ///
    /// Driven through a destination rather than through a real pipe deliberately: every
    /// generated script here is a few kilobytes and a pipe buffer is 64 KiB, so a spawned
    /// `| head` never makes the writer see `EPIPE` and the end-to-end version of this test
    /// would pass without exercising anything.
    #[test]
    fn a_closed_pipe_is_success() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let mut destination = Failing(io::ErrorKind::BrokenPipe);
            assert!(
                write(shell, &mut destination).is_ok(),
                "{shell:?}: a closed pipe should be success"
            );
        }
    }

    /// Every other write failure still is one, so the clause above cannot swallow a
    /// truncated script.
    #[test]
    fn a_write_that_failed_for_another_reason_is_reported() {
        let mut destination = Failing(io::ErrorKind::StorageFull);
        let error = write(Shell::Bash, &mut destination).expect_err("should have failed");
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }
}
