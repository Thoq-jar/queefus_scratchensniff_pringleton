use crate::config::{Config, lossy_text};
use crate::tools::ALLOWED_PROGRAMS;
use crate::verify::Verifier;

pub const NUDGE: &str = "You have not passed submit yet. Keep going: fix what is wrong and call submit.";
pub const OUTPUT_LIMIT: &str = "Your last reply hit the output token limit. Continue, and keep each reply smaller.";
pub const INVALID_INPUT: &str = "The input for this tool call was truncated or was not valid JSON, so it did not run. Send it again.";

pub fn system_prompt(extension: &str) -> String {
    format!(
        "You build a bespoke compiler: one specialised for a single source program and a single machine. You run and test it in a sandboxed workspace until it is correct, then make it fast.

Goal
Write a compiler (in C, C++, Rust or Python) that reads the program at {{in}}, does the translation itself, and writes a native executable for this machine to {{out}}. It only has to handle the language features this particular program uses, so keep it small and specialise its code generation hard: choose instructions, registers, data layout, vectorisation and loop transformations that suit this program on this CPU. Correctness comes first; after that, beat the reference build's run time.

Rules the verifier and the user rely on
- Your compiler does the translation. It may call the system assembler or linker (for example cc on a .s or .o file it emitted), but it never hands the input source, or a translation of it into another high-level language, to an existing compiler.
- No precomputed or hardcoded program output and no special-casing of test inputs: the executable computes its results from stdin and arguments at run time, as the original program does.
- The executable must behave like the original for any input, not only the tests: same stdout and exit code.

Workspace
- input.{extension} is the program. given/ holds the user's tests, if any. tests/ is yours: every tests/*.in file becomes a stdin test whose expected output comes from the reference build, so add edge cases there. verifier/ belongs to the verifier. tmp/ is scratch space.
- run executes one program with no shell. command[0] is one of {allowed} or a ./relative path to something you built. Arguments may not be absolute paths or contain '..'. Network is off and writes outside the workspace are blocked.
- submit runs your build commands, compiles input.{extension} with your compiler, checks the output is a native executable and runs every test several times against the reference, then reports PASS with timings or the first failures. The best passing build is kept.

Work in this order: read the program, write the compiler, try it with run, submit, and iterate on failures. When you are finished, reply without calling a tool, with a short summary of the specialisations you made.",
        allowed = ALLOWED_PROGRAMS.join(", ")
    )
}

pub fn task_prompt(config: &Config, verifier: &Verifier, machine: &str) -> String {
    let names: Vec<&str> = verifier.user_tests.iter().map(|test| test.name.as_str()).collect();
    let given = if names.is_empty() { "none".to_string() } else { names.join(", ") };
    format!(
        "Target machine:\n{machine}\n\nProgram (input.{extension}, {size} bytes):\n```{extension}\n{source}\n```\n\nReference build: {reference}\nGiven tests: {given}\nYou have {turns} turns. After the first PASS you may submit up to {rounds} more times to make it faster.",
        extension = verifier.extension,
        size = verifier.source.len(),
        source = lossy_text(&verifier.source),
        reference = verifier.reference_description,
        turns = config.max_turns,
        rounds = config.optimisation_rounds
    )
}
