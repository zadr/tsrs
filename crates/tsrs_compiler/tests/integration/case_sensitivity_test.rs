// Include diagnostics that depend on the host's case sensitivity, which the CLI takes from the volume of its
// executable and these tests from vfstest's flag. The expected text is tsgo 7.1.0-beta's `-p . --pretty false` output
// for the same files, run from a binary and project on an APFS volume of the same case sensitivity, with the project
// directory replaced by PROJECT.

use std::sync::Arc;

use tsrs_core::P;
use tsrs_core::tspath::ComparePathsOptions;
use tsrs_tsoptions::{self as tsoptions, ParseConfigHost};
use tsrs_vfs::{FS, bundled, vfstest};

use tsrs_compiler::diagnosticwriter::{FormattingOptions, write_format_diagnostic};
use tsrs_compiler::{
    Context, ProgramOptions, get_diagnostics_of_any_program, new_compiler_host, new_program,
};

const PROJECT: &str = "/home/src/workspaces/project";

const OPTIONS: &str = r#""target":"es2022","module":"esnext","moduleResolution":"bundler","strict":true,"types":[],"noEmit":true"#;

struct ParseConfigHostImpl {
    fs: Arc<dyn FS>,
}

impl ParseConfigHost for ParseConfigHostImpl {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &'static str {
        PROJECT
    }
}

fn tsc_output(files: &[(String, String)], use_case_sensitive_file_names: bool) -> String {
    let fs: Arc<dyn FS> = Arc::new(bundled::wrap_fs(vfstest::from_map(
        files.iter().map(|(name, text)| (format!("{PROJECT}/{name}"), text.as_str())),
        use_case_sensitive_file_names,
    )));
    let config_host: &'static ParseConfigHostImpl =
        Box::leak(Box::new(ParseConfigHostImpl { fs: Arc::clone(&fs) }));
    let (config, diagnostics) = tsoptions::get_parsed_command_line_of_config_file(
        &format!("{PROJECT}/tsconfig.json"),
        None,
        None,
        config_host,
        None,
    );
    assert_eq!(diagnostics, [] as [P<tsrs_ast::Diagnostic>; 0]);
    let program = new_program(ProgramOptions::new(
        P::new(config.unwrap()),
        new_compiler_host(PROJECT, fs, &bundled::lib_path(), None, None, None),
    ));
    let ctx = Context::default();
    let diagnostics = get_diagnostics_of_any_program(
        &ctx,
        program,
        None,
        false,
        &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
        &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
    );
    let format_opts = FormattingOptions {
        compare_paths_options: ComparePathsOptions {
            use_case_sensitive_file_names,
            current_directory: PROJECT.to_string(),
        },
        new_line: "\n".to_string(),
    };
    let mut output = Vec::new();
    for diagnostic in diagnostics {
        write_format_diagnostic(&mut output, diagnostic, &format_opts);
    }
    String::from_utf8(output).unwrap().replace(PROJECT, "PROJECT")
}

// `dir/y.ts` is a root file found after main.ts imported it as `other_casing/y.ts`, a second file on a case-sensitive
// host. The later include is not a reference and the first is, so the diagnostic is TS1261, not TS1149
// (includeprocessor.go:65): it must see the reasons recorded so far in the walk (filesparser.go:396), also when the
// case-insensitive lookup of a case-sensitive host finds the earlier file (filesparser.go:441).
fn root_file_after_import_with_other_casing(
    dir: &str,
    other_casing: &str,
    use_case_sensitive_file_names: bool,
) -> String {
    let y = "export const ly: string = \"s\";\n";
    let mut files = vec![
        ("tsconfig.json".to_string(), format!(r#"{{"compilerOptions":{{{OPTIONS}}}}}"#)),
        (format!("{dir}/x.ts"), "export const lx: number = 1;\n".to_string()),
        (format!("{dir}/y.ts"), y.to_string()),
        (
            "main.ts".to_string(),
            format!(
                "import {{ lx }} from \"./{dir}/x\"; import {{ ly }} from \"./{other_casing}/y\"; export const r: [number, string] = [lx, ly];\n"
            ),
        ),
    ];
    if use_case_sensitive_file_names {
        files.push((format!("{other_casing}/y.ts"), y.to_string()));
    }
    tsc_output(&files, use_case_sensitive_file_names)
}

#[test]
fn case_insensitive_host_root_file_after_import_with_other_casing() {
    assert_eq!(
        root_file_after_import_with_other_casing("lib", "Lib", false),
        "main.ts(1,50): error TS1261: Already included file name 'PROJECT/Lib/y.ts' differs from file name 'PROJECT/lib/y.ts' only in casing.
  The file is in the program because:
    Imported via \"./Lib/y\" from file 'PROJECT/main.ts'
    Matched by default include pattern '**/*'
"
    );
}

#[test]
fn case_sensitive_host_root_file_after_import_of_other_file_differing_in_casing() {
    assert_eq!(
        root_file_after_import_with_other_casing("lib", "Lib", true),
        "main.ts(1,50): error TS1261: Already included file name 'PROJECT/Lib/y.ts' differs from file name 'PROJECT/lib/y.ts' only in casing.
  The file is in the program because:
    Imported via \"./Lib/y\" from file 'PROJECT/main.ts'
    Matched by default include pattern '**/*'
    Matched by default include pattern '**/*'
"
    );
}

#[test]
fn case_insensitive_host_root_file_after_import_with_other_non_ascii_casing() {
    assert_eq!(
        root_file_after_import_with_other_casing("é", "É", false),
        "main.ts(1,48): error TS1261: Already included file name 'PROJECT/É/y.ts' differs from file name 'PROJECT/é/y.ts' only in casing.
  The file is in the program because:
    Imported via \"./É/y\" from file 'PROJECT/main.ts'
    Matched by default include pattern '**/*'
"
    );
}

#[test]
fn case_sensitive_host_root_file_after_import_of_other_file_differing_in_non_ascii_casing() {
    assert_eq!(
        root_file_after_import_with_other_casing("é", "É", true),
        "main.ts(1,48): error TS1261: Already included file name 'PROJECT/É/y.ts' differs from file name 'PROJECT/é/y.ts' only in casing.
  The file is in the program because:
    Imported via \"./É/y\" from file 'PROJECT/main.ts'
    Matched by default include pattern '**/*'
    Matched by default include pattern '**/*'
"
    );
}
