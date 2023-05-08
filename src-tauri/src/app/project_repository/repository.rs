use std::{collections::HashMap, env};

use anyhow::{Context, Result};
use serde::Serialize;
use walkdir::WalkDir;

use crate::{
    app::{project_repository::activity, reader},
    projects,
};

pub struct Repository<'repository> {
    pub(crate) git_repository: git2::Repository,
    project: &'repository projects::Project,
}

impl<'repository> Repository<'repository> {
    pub fn open(project: &'repository projects::Project) -> Result<Self> {
        let git_repository = git2::Repository::open(&project.path)
            .with_context(|| format!("{}: failed to open git repository", project.path))?;
        Ok(Self {
            git_repository,
            project,
        })
    }

    pub fn get_project(&self) -> &'repository projects::Project {
        self.project
    }

    pub fn get_head(&self) -> Result<git2::Reference> {
        let head = self.git_repository.head()?;
        Ok(head)
    }

    pub fn is_path_ignored<P: AsRef<std::path::Path>>(&self, path: P) -> Result<bool> {
        let path = path.as_ref();
        let ignored = self.git_repository.is_path_ignored(path)?;
        Ok(ignored)
    }

    pub fn get_wd_reader(&self) -> reader::DirReader {
        reader::DirReader::open(self.root().to_path_buf())
    }

    pub fn get_head_reader(&self) -> Result<reader::CommitReader> {
        let head = self.git_repository.head()?;
        let commit = head.peel_to_commit()?;
        let reader = reader::CommitReader::from_commit(&self.git_repository, commit)?;
        Ok(reader)
    }

    pub fn root(&self) -> &std::path::Path {
        self.git_repository.path().parent().unwrap()
    }

    pub fn git_activity(
        &self,
        start_time_ms: Option<u128>,
    ) -> Result<Vec<activity::Activity>> {
        let head_logs_path = self.git_repository.path().join("logs").join("HEAD");

        if !head_logs_path.exists() {
            return Ok(Vec::new());
        }

        let activity = std::fs::read_to_string(head_logs_path)
            .with_context(|| "failed to read HEAD logs")?
            .lines()
            .filter_map(|line| activity::parse_reflog_line(line).ok())
            .collect::<Vec<activity::Activity>>();

        let activity = if let Some(start_timestamp_ms) = start_time_ms {
            activity
                .into_iter()
                .filter(|activity| activity.timestamp_ms > start_timestamp_ms)
                .collect::<Vec<activity::Activity>>()
        } else {
            activity
        };

        Ok(activity)
    }

    fn unstaged_statuses(&self) -> Result<HashMap<String, FileStatusType>> {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        options.recurse_untracked_dirs(true);
        options.include_ignored(false);
        options.show(git2::StatusShow::Workdir);

        // get the status of the repository
        let statuses = self
            .git_repository
            .statuses(Some(&mut options))
            .with_context(|| "failed to get repository status")?;

        let files = statuses
            .iter()
            .filter_map(|entry| {
                entry
                    .path()
                    .map(|path| (path.to_string(), FileStatusType::from(entry.status())))
            })
            .collect();

        return Ok(files);
    }

    fn staged_statuses(&self) -> Result<HashMap<String, FileStatusType>> {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true);
        options.include_ignored(false);
        options.recurse_untracked_dirs(true);
        options.show(git2::StatusShow::Index);

        // get the status of the repository
        let statuses = self
            .git_repository
            .statuses(Some(&mut options))
            .with_context(|| "failed to get repository status")?;

        let files = statuses
            .iter()
            .filter_map(|entry| {
                entry
                    .path()
                    .map(|path| (path.to_string(), FileStatusType::from(entry.status())))
            })
            .collect();

        return Ok(files);
    }

    pub fn git_status(&self) -> Result<HashMap<String, FileStatus>> {
        let staged_statuses = self.staged_statuses()?;
        let unstaged_statuses = self.unstaged_statuses()?;
        let mut statuses = HashMap::new();
        unstaged_statuses
            .iter()
            .for_each(|(path, unstaged_status_type)| {
                statuses.insert(
                    path.clone(),
                    FileStatus {
                        unstaged: Some(*unstaged_status_type),
                        staged: None,
                    },
                );
            });
        staged_statuses
            .iter()
            .for_each(|(path, stages_status_type)| {
                if let Some(status) = statuses.get_mut(path) {
                    status.staged = Some(*stages_status_type);
                } else {
                    statuses.insert(
                        path.clone(),
                        FileStatus {
                            unstaged: None,
                            staged: Some(*stages_status_type),
                        },
                    );
                }
            });

        Ok(statuses)
    }

    pub fn git_wd_diff(&self, context_lines: usize) -> Result<HashMap<String, String>> {
        let head = self.git_repository.head()?;
        let tree = head.peel_to_tree()?;

        // Prepare our diff options based on the arguments given
        let mut opts = git2::DiffOptions::new();
        opts.recurse_untracked_dirs(true)
            .include_untracked(true)
            .show_untracked_content(true)
            .context_lines(if context_lines == 0 {
                3
            } else {
                context_lines.try_into()?
            });

        let diff = self
            .git_repository
            .diff_tree_to_workdir(Some(&tree), Some(&mut opts))?;

        let mut result = HashMap::new();
        let mut results = String::new();

        let mut current_line_count = 0;
        let mut last_path = String::new();

        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            let new_path = delta
                .new_file()
                .path()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            if new_path != last_path {
                result.insert(last_path.clone(), results.clone());
                results = String::new();
                current_line_count = 0;
                last_path = new_path.clone();
            }
            match line.origin() {
                '+' | '-' | ' ' => results.push_str(&format!("{}", line.origin())),
                _ => {}
            }
            results.push_str(&format!("{}", std::str::from_utf8(line.content()).unwrap()));
            current_line_count += 1;
            true
        })?;
        result.insert(last_path.clone(), results.clone());
        Ok(result)
    }

    pub fn git_match_paths(&self, pattern: &str) -> Result<Vec<String>> {
        let workdir = self
            .git_repository
            .workdir()
            .with_context(|| "failed to get working directory")?;

        let pattern = pattern.to_lowercase();
        let mut files = vec![];
        for entry in WalkDir::new(workdir)
                    .into_iter()
                    .filter_entry(|entry| {
                        // need to remove workdir so we're not matching it
                        let relative_path = entry
                            .path()
                            .strip_prefix(workdir)
                            .unwrap()
                            .to_str()
                            .unwrap();
                        // this is to make it faster, so we dont have to traverse every directory if it is ignored by git
                        entry.path().to_str() == workdir.to_str()  // but we need to traverse the first one
                            || ((entry.file_type().is_dir() // traverse all directories if they are not ignored by git
                                || relative_path.to_lowercase().contains(&pattern)) // but only pass on files that match the regex
                                && !self.git_repository.is_path_ignored(&entry.path()).unwrap_or(true))
                    })
                    .filter_map(Result::ok)
                {
                    if entry.file_type().is_file() {
                        // only save the matching files, not the directories
                        let path = entry.path();
                        let path = path
                            .strip_prefix::<&std::path::Path>(workdir.as_ref())
                            .with_context(|| {
                                format!(
                                    "failed to strip prefix from path {}",
                                    path.to_str().unwrap()
                                )
                            })?;
                        let path = path.to_str().unwrap().to_string();
                        files.push(path);
                    }
                }
        files.sort();
        return Ok(files);
    }

    pub fn git_branches(&self) -> Result<Vec<String>> {
        let mut branches = vec![];
        for branch in self
            .git_repository
            .branches(Some(git2::BranchType::Local))?
        {
            let (branch, _) = branch?;
            let name = branch.name()?.unwrap().to_string();
            branches.push(name);
        }
        Ok(branches)
    }

    pub fn git_switch_branch(&self, branch: &str) -> Result<()> {
        let branch = self
            .git_repository
            .find_branch(branch, git2::BranchType::Local)?;
        let branch = branch.into_reference();
        self.git_repository
            .set_head(branch.name().unwrap())
            .context("failed to set head")?;
        self.git_repository
            .checkout_head(Some(&mut git2::build::CheckoutBuilder::default().force()))
            .context("failed to checkout head")?;
        Ok(())
    }

    pub fn git_stage_files<P: AsRef<std::path::Path>>(&self, paths: Vec<P>) -> Result<()> {
        let mut index = self.git_repository.index()?;
        for path in paths {
            let path = path.as_ref();
            // to "stage" a file means to:
            // - remove it from the index if file is deleted
            // - overwrite it in the index otherwise
            if !std::path::Path::new(&self.project.path).join(path).exists() {
                index.remove_path(path).with_context(|| {
                    format!("failed to remove path {} from index", path.display())
                })?;
            } else {
                index
                    .add_path(path)
                    .with_context(|| format!("failed to add path {} to index", path.display()))?;
            }
        }
        index.write().with_context(|| "failed to write index")?;
        Ok(())
    }

    pub fn git_unstage_files<P: AsRef<std::path::Path>>(&self, paths: Vec<P>) -> Result<()> {
        let head_tree = self.git_repository.head()?.peel_to_tree()?;
        let mut head_index = git2::Index::new()?;
        head_index.read_tree(&head_tree)?;
        let mut index = self.git_repository.index()?;
        for path in paths {
            let path = path.as_ref();
            // to "unstage" a file means to:
            // - put head version of the file in the index if it exists
            // - remove it from the index otherwise
            let head_index_entry = head_index.iter().find(|entry| {
                let entry_path = String::from_utf8(entry.path.clone());
                entry_path.as_ref().unwrap() == path.to_str().unwrap()
            });
            if let Some(entry) = head_index_entry {
                index
                    .add(&entry)
                    .with_context(|| format!("failed to add path {} to index", path.display()))?;
            } else {
                index.remove_path(path).with_context(|| {
                    format!("failed to remove path {} from index", path.display())
                })?;
            }
        }
        index.write().with_context(|| "failed to write index")?;
        Ok(())
    }

    pub fn git_commit(&self, message: &str, push: bool) -> Result<()> {
        let config = self
            .git_repository
            .config()
            .with_context(|| "failed to get config")?;
        let name = config
            .get_string("user.name")
            .with_context(|| "failed to get user.name")?;
        let email = config
            .get_string("user.email")
            .with_context(|| "failed to get user.email")?;

        // Get the default signature for the repository
        let signature =
            git2::Signature::now(&name, &email).with_context(|| "failed to get signature")?;

        // Create the commit with current index
        let tree_id = self.git_repository.index()?.write_tree()?;
        let tree = self.git_repository.find_tree(tree_id)?;
        let parent_commit = self.git_repository.head()?.peel_to_commit()?;
        let commit = self.git_repository.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &[&parent_commit],
        )?;
        log::info!("{}: created commit {}", self.project.id, commit);

        if push {
            // Get a reference to the current branch
            let head = self.git_repository.head()?;
            let branch = head.name().unwrap();

            let branch_remote = self.git_repository.branch_upstream_remote(branch)?;
            let branch_remote_name = branch_remote.as_str().unwrap();
            let branch_name = self.git_repository.branch_upstream_name(branch)?;

            log::info!(
                "{}: pushing {} to {} as {}",
                self.project.id,
                branch,
                branch_remote_name,
                branch_name.as_str().unwrap()
            );

            // Set the remote's callbacks
            let mut callbacks = git2::RemoteCallbacks::new();

            callbacks.push_update_reference(move |refname, message| {
                log::info!("pushing reference '{}': {:?}", refname, message);
                Ok(())
            });
            callbacks.push_transfer_progress(move |one, two, three| {
                log::info!("transferred {}/{}/{} objects", one, two, three);
            });

            // create ssh key if it's not there

            // try to auth with creds from an ssh-agent
            callbacks.credentials(|_url, username_from_url, _allowed_types| {
                git2::Cred::ssh_key(
                    username_from_url.unwrap(),
                    None,
                    std::path::Path::new(&format!("{}/.ssh/id_ed25519", env::var("HOME").unwrap())),
                    None,
                )
            });

            let mut push_options = git2::PushOptions::new();
            push_options.remote_callbacks(callbacks);

            // Push to the remote
            let mut remote = self.git_repository.find_remote(branch_remote_name)?;
            remote
                .push(&[branch], Some(&mut push_options))
                .with_context(|| {
                    format!("failed to push {:?} to {:?}", branch, branch_remote_name)
                })?;
        }

        return Ok(());
    }
}

#[derive(Serialize, Copy, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub staged: Option<FileStatusType>,
    pub unstaged: Option<FileStatusType>,
}

#[derive(Serialize, Copy, Clone)]
#[serde(rename_all = "camelCase")]
pub enum FileStatusType {
    Added,
    Modified,
    Deleted,
    Renamed,
    TypeChange,
    Other,
}

impl From<git2::Status> for FileStatusType {
    fn from(status: git2::Status) -> Self {
        if status.is_index_new() || status.is_wt_new() {
            FileStatusType::Added
        } else if status.is_index_modified() || status.is_wt_modified() {
            FileStatusType::Modified
        } else if status.is_index_deleted() || status.is_wt_deleted() {
            FileStatusType::Deleted
        } else if status.is_index_renamed() || status.is_wt_renamed() {
            FileStatusType::Renamed
        } else if status.is_index_typechange() || status.is_wt_typechange() {
            FileStatusType::TypeChange
        } else {
            FileStatusType::Other
        }
    }
}