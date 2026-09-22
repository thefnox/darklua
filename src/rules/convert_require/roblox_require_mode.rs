use serde::{Deserialize, Serialize};

use crate::{
    frontend::DarkluaResult,
    nodes::{Arguments, FunctionCall, Prefix},
    rules::{
        convert_require::rojo_sourcemap::RojoSourcemap,
        require::path_utils::{get_relative_parent_path, get_relative_path},
        Context,
    },
    utils, DarkluaError,
};

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{
    instance_path::{get_parent_instance, script_identifier},
    RequireMode, RobloxIndexStyle,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RobloxRequireMode {
    rojo_sourcemap: Option<PathBuf>,
    #[serde(default, deserialize_with = "crate::utils::string_or_struct")]
    indexing_style: RobloxIndexStyle,
    #[serde(skip)]
    cached_sourcemap: Option<Arc<RojoSourcemap>>,
    #[serde(skip)]
    parsed_sourcemap: ParsedSourcemap,
}

impl RobloxRequireMode {
    pub(crate) fn initialize(&mut self, context: &Context) -> DarkluaResult<()> {
        if let Some(ref rojo_sourcemap_path) = self
            .rojo_sourcemap
            .as_ref()
            .map(|rojo_sourcemap_path| context.project_location().join(rojo_sourcemap_path))
        {
            context.add_file_dependency(rojo_sourcemap_path.clone());

            let content = context
                .resources()
                .get(rojo_sourcemap_path)
                .map_err(|err| {
                    DarkluaError::from(err).context("while initializing Roblox require mode")
                })?;
            let sourcemap = self
                .parsed_sourcemap
                .get_or_parse(rojo_sourcemap_path, content)
                .map_err(|err| {
                    err.context(format!(
                        "unable to parse Rojo sourcemap at `{}`",
                        rojo_sourcemap_path.display()
                    ))
                })?;
            self.cached_sourcemap = Some(sourcemap);
        }
        Ok(())
    }

    pub(crate) fn find_require(
        &self,
        _call: &FunctionCall,
        _context: &Context,
    ) -> DarkluaResult<Option<PathBuf>> {
        Err(DarkluaError::custom("unsupported initial require mode")
            .context("Roblox require mode cannot be used as the current require mode"))
    }

    pub(crate) fn generate_require(
        &self,
        require_path: &Path,
        current: &RequireMode,
        context: &Context,
    ) -> DarkluaResult<Option<Arguments>> {
        let source_path = utils::normalize_path(context.current_path());
        log::trace!(
            "generate Roblox require for `{}` from `{}`",
            require_path.display(),
            source_path.display(),
        );

        if let Some((sourcemap, sourcemap_path)) = self
            .cached_sourcemap
            .as_ref()
            .zip(self.rojo_sourcemap.as_ref())
        {
            if let Some(require_relative_to_sourcemap) = get_relative_path(
                require_path,
                get_relative_parent_path(sourcemap_path),
                false,
            )? {
                log::trace!(
                    "  ⨽ use sourcemap at `{}` to find `{}`",
                    sourcemap_path.display(),
                    require_relative_to_sourcemap.display()
                );

                if let Some(instance_path) =
                    sourcemap.get_instance_path(&source_path, &require_relative_to_sourcemap)
                {
                    Ok(Some(Arguments::default().with_argument(
                        instance_path.convert(&self.indexing_style),
                    )))
                } else {
                    match (
                        sourcemap.exists(&source_path),
                        sourcemap.exists(&require_relative_to_sourcemap),
                    ) {
                        (true, true) => {
                            log::warn!(
                                "unable to get relative path to `{}` in sourcemap (from `{}`)",
                                require_relative_to_sourcemap.display(),
                                source_path.display()
                            );
                        }
                        (false, _) => {
                            log::warn!(
                                "unable to find source path `{}` in sourcemap",
                                source_path.display()
                            );
                        }
                        (true, false) => {
                            log::warn!(
                                "unable to find path `{}` in sourcemap (from `{}`)",
                                require_relative_to_sourcemap.display(),
                                source_path.display()
                            );
                        }
                    }
                    Ok(None)
                }
            } else {
                log::debug!(
                    "unable to get relative path from sourcemap for `{}`",
                    require_path.display()
                );
                Ok(None)
            }
        } else if let Some(relative_require_path) =
            get_relative_path(require_path, &source_path, true)?
        {
            log::trace!(
                "make require path relative to source: `{}`",
                relative_require_path.display()
            );

            let require_is_module_folder_name =
                current.is_module_folder_name(&relative_require_path);
            // if we are about to make a require to a path like `./x/y/z/init.lua`
            // we can pop the last component from the path
            let take_components = relative_require_path
                .components()
                .count()
                .saturating_sub(if require_is_module_folder_name { 1 } else { 0 });
            let mut path_components = relative_require_path.components().take(take_components);

            if let Some(first_component) = path_components.next() {
                let source_is_module_folder_name = current.is_module_folder_name(&source_path);

                let instance_path = path_components.try_fold(
                    match first_component {
                        Component::CurDir => {
                            if source_is_module_folder_name {
                                script_identifier().into()
                            } else {
                                get_parent_instance(script_identifier())
                            }
                        }
                        Component::ParentDir => {
                            if source_is_module_folder_name {
                                get_parent_instance(script_identifier())
                            } else {
                                get_parent_instance(get_parent_instance(script_identifier()))
                            }
                        }
                        Component::Normal(_) => {
                            return Err(DarkluaError::custom(format!(
                                concat!(
                                    "unable to convert path `{}`: the require path should be ",
                                    "relative and start with `.` or `..` (got `{}`)"
                                ),
                                require_path.display(),
                                relative_require_path.display(),
                            )))
                        }
                        Component::Prefix(_) | Component::RootDir => {
                            return Err(DarkluaError::custom(format!(
                                concat!(
                                    "unable to convert absolute path `{}`: ",
                                    "without a provided Rojo sourcemap, ",
                                    "darklua can only convert relative paths ",
                                    "(starting with `.` or `..`)"
                                ),
                                require_path.display(),
                            )))
                        }
                    },
                    |instance: Prefix, component| match component {
                        Component::CurDir => Ok(instance),
                        Component::ParentDir => Ok(get_parent_instance(instance)),
                        Component::Normal(name) => utils::convert_os_string(name)
                            .map(|child_name| self.indexing_style.index(instance, child_name)),
                        Component::Prefix(_) | Component::RootDir => {
                            Err(DarkluaError::custom(format!(
                                "unable to convert path `{}`: unexpected component in relative path `{}`",
                                require_path.display(),
                                relative_require_path.display(),
                            )))
                        },
                    },
                )?;

                Ok(Some(Arguments::default().with_argument(instance_path)))
            } else {
                Err(DarkluaError::custom(format!(
                    "unable to convert path `{}` from `{}` without a sourcemap: the relative path is empty `{}`",
                    require_path.display(),
                    source_path.display(),
                    relative_require_path.display(),
                )))
            }
        } else {
            Err(DarkluaError::custom(format!(
                concat!(
                    "unable to convert path `{}` from `{}` without a sourcemap: unable to ",
                    "make the require path relative to the source file"
                ),
                require_path.display(),
                source_path.display(),
            )))
        }
    }
}

/// The last sourcemap parsed by a require mode. The rule clones its require mode for
/// each processed file, and the clones share this value, so the sourcemap is only
/// parsed again when its path or content changes.
#[derive(Clone, Default)]
struct ParsedSourcemap(Arc<Mutex<Option<ParsedSourcemapEntry>>>);

struct ParsedSourcemapEntry {
    path: PathBuf,
    content: String,
    sourcemap: Arc<RojoSourcemap>,
}

impl ParsedSourcemap {
    fn get_or_parse(&self, path: &Path, content: String) -> DarkluaResult<Arc<RojoSourcemap>> {
        let mut entry = self.0.lock().unwrap();

        if let Some(entry) = entry
            .as_ref()
            .filter(|entry| entry.path == path && entry.content == content)
        {
            return Ok(Arc::clone(&entry.sourcemap));
        }

        let sourcemap = Arc::new(RojoSourcemap::parse(
            &content,
            get_relative_parent_path(path),
        )?);
        *entry = Some(ParsedSourcemapEntry {
            path: path.to_path_buf(),
            content,
            sourcemap: Arc::clone(&sourcemap),
        });
        Ok(sourcemap)
    }
}

impl fmt::Debug for ParsedSourcemap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedSourcemap").finish_non_exhaustive()
    }
}

// a cache does not change how a require mode behaves
impl PartialEq for ParsedSourcemap {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ParsedSourcemap {}

#[cfg(test)]
mod test {
    use super::*;

    const SOURCEMAP: &str =
        r#"{ "name": "Project", "className": "ModuleScript", "filePaths": ["src/init.lua"] }"#;

    #[test]
    fn parsed_sourcemap_is_reused_while_content_is_unchanged() {
        let parsed_sourcemap = ParsedSourcemap::default();
        let path = Path::new("sourcemap.json");

        let first = parsed_sourcemap
            .get_or_parse(path, SOURCEMAP.to_owned())
            .unwrap();
        let second = parsed_sourcemap
            .clone()
            .get_or_parse(path, SOURCEMAP.to_owned())
            .unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn parsed_sourcemap_is_parsed_again_when_content_changes() {
        let parsed_sourcemap = ParsedSourcemap::default();
        let path = Path::new("sourcemap.json");

        let first = parsed_sourcemap
            .get_or_parse(path, SOURCEMAP.to_owned())
            .unwrap();
        let second = parsed_sourcemap
            .get_or_parse(path, SOURCEMAP.replace("init.lua", "main.lua"))
            .unwrap();

        assert!(!Arc::ptr_eq(&first, &second));
        assert!(second.exists(Path::new("src/main.lua")));
        assert!(!second.exists(Path::new("src/init.lua")));
    }
}
