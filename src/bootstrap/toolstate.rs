use serde::{Deserialize, Serialize};
use build_helper::t;
use std::time;
use std::fs;
use std::io::{Seek, SeekFrom};
use std::collections::HashMap;
use crate::builder::{Builder, RunConfig, ShouldRun, Step};
use std::fmt;
use std::process::Command;
use std::path::PathBuf;
use std::env;

// Each cycle is 42 days long (6 weeks); the last week is 35..=42 then.
const BETA_WEEK_START: u64 = 35;

#[cfg(linux)]
const OS: Option<&str> = Some("linux");

#[cfg(windows)]
const OS: Option<&str> = Some("windows");

#[cfg(all(not(linux), not(windows)))]
const OS: Option<&str> = None;

type ToolstateData = HashMap<Box<str>, ToolState>;

#[derive(Copy, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
/// Whether a tool can be compiled, tested or neither
pub enum ToolState {
    /// The tool compiles successfully, but the test suite fails
    TestFail = 1,
    /// The tool compiles successfully and its test suite passes
    TestPass = 2,
    /// The tool can't even be compiled
    BuildFail = 0,
}

impl fmt::Display for ToolState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            ToolState::TestFail => "test-fail",
            ToolState::TestPass => "test-pass",
            ToolState::BuildFail => "build-fail",
        })
    }
}

impl Default for ToolState {
    fn default() -> Self {
        // err on the safe side
        ToolState::BuildFail
    }
}

/// Number of days after the last promotion of beta.
/// Its value is 41 on the Tuesday where "Promote master to beta (T-2)" happens.
/// The Wednesday after this has value 0.
/// We track this value to prevent regressing tools in the last week of the 6-week cycle.
fn days_since_beta_promotion() -> u64 {
    let since_epoch = t!(time::SystemTime::UNIX_EPOCH.elapsed());
    (since_epoch.as_secs() / 86400 - 20) % 42
}

// These tools must test-pass on the beta/stable channels.
//
// On the nightly channel, their build step must be attempted, but they may not
// be able to build successfully.
static STABLE_TOOLS: &[(&str, &str)] = &[
    ("book", "src/doc/book"),
    ("nomicon", "src/doc/nomicon"),
    ("reference", "src/doc/reference"),
    ("rust-by-example", "src/doc/rust-by-example"),
    ("edition-guide", "src/doc/edition-guide"),
    ("rls", "src/tools/rls"),
    ("rustfmt", "src/tools/rustfmt"),
    ("clippy-driver", "src/tools/clippy"),
];

// These tools are permitted to not build on the beta/stable channels.
//
// We do require that we checked whether they build or not on the tools builder,
// though, as otherwise we will be unable to file an issue if they start
// failing.
static NIGHTLY_TOOLS: &[(&str, &str)] = &[
    ("miri", "src/tools/miri"),
    ("embedded-book", "src/doc/embedded-book"),
    ("rustc-guide", "src/doc/rustc-guide"),
];

fn print_error(tool: &str, submodule: &str) {
    eprintln!("");
    eprintln!("We detected that this PR updated '{}', but its tests failed.", tool);
    eprintln!("");
    eprintln!("If you do intend to update '{}', please check the error messages above and", tool);
    eprintln!("commit another update.");
    eprintln!("");
    eprintln!("If you do NOT intend to update '{}', please ensure you did not accidentally", tool);
    eprintln!("change the submodule at '{}'. You may ask your reviewer for the", submodule);
    eprintln!("proper steps.");
    std::process::exit(3);
}

fn check_changed_files(toolstates: &HashMap<Box<str>, ToolState>) {
    // Changed files
    let output = std::process::Command::new("git")
        .arg("diff")
        .arg("--name-status")
        .arg("HEAD")
        .arg("HEAD^")
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to get changed files: {:?}", e);
            std::process::exit(1);
        }
    };

    let output = t!(String::from_utf8(output.stdout));

    for (tool, submodule) in STABLE_TOOLS.iter().chain(NIGHTLY_TOOLS.iter()) {
        let changed = output.lines().any(|l| {
            l.starts_with("M") && l.ends_with(submodule)
        });
        eprintln!("Verifying status of {}...", tool);
        if !changed {
            continue;
        }

        eprintln!("This PR updated '{}', verifying if status is 'test-pass'...", submodule);
        if toolstates[*tool] != ToolState::TestPass {
            print_error(tool, submodule);
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ToolStateCheck;

impl Step for ToolStateCheck {
    type Output = ();

    /// Runs the `linkchecker` tool as compiled in `stage` by the `host` compiler.
    ///
    /// This tool in `src/tools` will verify the validity of all our links in the
    /// documentation to ensure we don't have a bunch of dead ones.
    fn run(self, builder: &Builder<'_>) {
        if builder.config.dry_run {
            return;
        }

        let days_since_beta_promotion = days_since_beta_promotion();
        let in_beta_week = days_since_beta_promotion >= BETA_WEEK_START;
        let is_nightly = !(builder.config.channel == "beta" || builder.config.channel == "stable");
        let toolstates = builder.toolstates();

        let mut did_error = false;

        for (tool, _) in STABLE_TOOLS.iter().chain(NIGHTLY_TOOLS.iter()) {
            if !toolstates.contains_key(*tool) {
                did_error = true;
                eprintln!("error: Tool `{}` was not recorded in tool state.", tool);
            }
        }

        if did_error {
            std::process::exit(1);
        }

        check_changed_files(&toolstates);

        for (tool, _) in STABLE_TOOLS.iter() {
            let state = toolstates[*tool];

            if state != ToolState::TestPass {
                if !is_nightly {
                    did_error = true;
                    eprintln!("error: Tool `{}` should be test-pass but is {}", tool, state);
                } else if in_beta_week {
                    did_error = true;
                    eprintln!("error: Tool `{}` should be test-pass but is {} during beta week.",
                        tool, state);
                }
            }
        }

        if did_error {
            std::process::exit(1);
        }

        if builder.config.channel == "nightly" && env::var_os("TOOLSTATE_PUBLISH").is_some() {
            commit_toolstate_change(&toolstates, in_beta_week);
        }
    }

    fn should_run(run: ShouldRun<'_>) -> ShouldRun<'_> {
        run.path("check-tools")
    }

    fn make_run(run: RunConfig<'_>) {
        run.builder.ensure(ToolStateCheck);
    }
}

impl Builder<'_> {
    fn toolstates(&self) -> HashMap<Box<str>, ToolState> {
        if let Some(ref path) = self.config.save_toolstates {
            if let Some(parent) = path.parent() {
                // Ensure the parent directory always exists
                t!(std::fs::create_dir_all(parent));
            }
            let mut file = t!(fs::OpenOptions::new()
                .create(true)
                .write(true)
                .read(true)
                .open(path));

            serde_json::from_reader(&mut file).unwrap_or_default()
        } else {
            Default::default()
        }
    }

    /// Updates the actual toolstate of a tool.
    ///
    /// The toolstates are saved to the file specified by the key
    /// `rust.save-toolstates` in `config.toml`. If unspecified, nothing will be
    /// done. The file is updated immediately after this function completes.
    pub fn save_toolstate(&self, tool: &str, state: ToolState) {
        if let Some(ref path) = self.config.save_toolstates {
            if let Some(parent) = path.parent() {
                // Ensure the parent directory always exists
                t!(std::fs::create_dir_all(parent));
            }
            let mut file = t!(fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(path));

            let mut current_toolstates: HashMap<Box<str>, ToolState> =
                serde_json::from_reader(&mut file).unwrap_or_default();
            current_toolstates.insert(tool.into(), state);
            t!(file.seek(SeekFrom::Start(0)));
            t!(file.set_len(0));
            t!(serde_json::to_writer(file, &current_toolstates));
        }
    }
}

/// This function `commit_toolstate_change` provides functionality for pushing a change
/// to the `rust-toolstate` repository.
///
/// The function relies on a GitHub bot user, which should have a Personal access
/// token defined in the environment variable $TOOLSTATE_REPO_ACCESS_TOKEN. If for
/// some reason you need to change the token, please update the Azure Pipelines
/// variable group.
///
///   1. Generate a new Personal access token:
///
///       * Login to the bot account, and go to Settings -> Developer settings ->
///           Personal access tokens
///       * Click "Generate new token"
///       * Enable the "public_repo" permission, then click "Generate token"
///       * Copy the generated token (should be a 40-digit hexadecimal number).
///           Save it somewhere secure, as the token would be gone once you leave
///           the page.
///
///   2. Update the variable group in Azure Pipelines
///
///       * Ping a member of the infrastructure team to do this.
///
///   4. Replace the email address below if the bot account identity is changed
///
///       * See <https://help.github.com/articles/about-commit-email-addresses/>
///           if a private email by GitHub is wanted.
fn commit_toolstate_change(
    current_toolstate: &ToolstateData,
    in_beta_week: bool,
) {
    fn git_config(key: &str, value: &str) {
        let status = Command::new("git").arg("config").arg("--global").arg(key).arg(value).status();
        let success = match status {
            Ok(s) => s.success(),
            Err(_) => false,
        };
        if !success {
            panic!("git config key={} value={} successful (status: {:?})", key, value, status);
        }
    }

    // If changing anything here, then please check that src/ci/publish_toolstate.sh is up to date
    // as well.
    git_config("user.email", "7378925+rust-toolstate-update@users.noreply.github.com");
    git_config("user.name", "Rust Toolstate Update");
    git_config("credential.helper", "store");

    let credential = format!(
        "https://{}:x-oauth-basic@github.com\n",
        t!(env::var("TOOLSTATE_REPO_ACCESS_TOKEN")),
    );
    let git_credential_path = PathBuf::from(t!(env::var("HOME"))).join(".git-credentials");
    t!(fs::write(&git_credential_path, credential));

    let status = Command::new("git").arg("clone")
        .arg("--depth=1")
        .arg(t!(env::var("TOOLSTATE_REPO")))
        .status();
    let success = match status {
        Ok(s) => s.success(),
        Err(_) => false,
    };
    if !success {
        panic!("git clone successful (status: {:?})", status);
    }

    let old_toolstate = t!(fs::read("rust-toolstate/_data/latest.json"));
    let old_toolstate: Vec<RepoState> = t!(serde_json::from_slice(&old_toolstate));

    let message = format!("({} CI update)", OS.expect("linux/windows only"));
    let mut success = false;
    for _ in 1..=5 {
        // Update the toolstate results (the new commit-to-toolstate mapping) in the toolstate repo.
        change_toolstate(&current_toolstate, &old_toolstate, in_beta_week);

        // `git commit` failing means nothing to commit.
        let status = t!(Command::new("git")
            .current_dir("rust-toolstate")
            .arg("commit")
            .arg("-a")
            .arg("-m")
            .arg(&message)
            .status());
        if !status.success() {
            success = true;
            break;
        }

        let status = t!(Command::new("git")
            .current_dir("rust-toolstate")
            .arg("push")
            .arg("origin")
            .arg("master")
            .status());
        // If we successfully push, exit.
        if status.success() {
            success = true;
            break;
        }
        eprintln!("Sleeping for 3 seconds before retrying push");
        std::thread::sleep(std::time::Duration::from_secs(3));
        let status = t!(Command::new("git")
            .current_dir("rust-toolstate")
            .arg("fetch")
            .arg("origin")
            .arg("master")
            .status());
        assert!(status.success());
        let status = t!(Command::new("git")
            .current_dir("rust-toolstate")
            .arg("reset")
            .arg("--hard")
            .arg("origin/master")
            .status());
        assert!(status.success());
    }

    if !success {
        panic!("Failed to update toolstate repository with new data");
    }
}

fn change_toolstate(
    current_toolstate: &ToolstateData,
    old_toolstate: &[RepoState],
    in_beta_week: bool,
) {
    let mut regressed = false;
    for repo_state in old_toolstate {
        let tool = &repo_state.tool;
        let state = if cfg!(linux) {
            &repo_state.linux
        } else if cfg!(windows) {
            &repo_state.windows
        } else {
            unimplemented!()
        };
        let new_state = current_toolstate[tool.as_str()];

        if new_state != *state {
            eprintln!("The state of `{}` has changed from `{}` to `{}`", tool, state, new_state);
            if (new_state as u8) < (*state as u8) {
                if !["rustc-guide", "miri", "embedded-book"].contains(&tool.as_str()) {
                    regressed = true;
                }
            }
        }
    }

    if regressed && in_beta_week {
        std::process::exit(1);
    }

    let commit = t!(std::process::Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .output());
    let commit = t!(String::from_utf8(commit.stdout));

    let toolstate_serialized = t!(serde_json::to_string(&current_toolstate));

    let history_path = format!("rust-toolstate/history/{}.tsv", OS.expect("linux/windows only"));
    let mut file = t!(fs::read_to_string(&history_path));
    let end_of_first_line = file.find('\n').unwrap();
    file.insert_str(end_of_first_line, &format!("{}\t{}\n", commit, toolstate_serialized));
    t!(fs::write(&history_path, file));
}

#[derive(Debug, Serialize, Deserialize)]
struct RepoState {
    tool: String,
    windows: ToolState,
    linux: ToolState,
    commit: String,
    datetime: String,
}
