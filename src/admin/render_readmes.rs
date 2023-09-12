use crate::{
    db,
    models::Version,
    schema::{crates, readme_renderings, versions},
};
use anyhow::{anyhow, Context};
use std::{io::Read, path::Path, sync::Arc, thread};

use crate::storage::Storage;
use chrono::{TimeZone, Utc};
use crates_io_markdown::text_to_html;
use crates_io_tarball::Manifest;
use diesel::prelude::*;
use flate2::read::GzDecoder;
use reqwest::{blocking::Client, header};
use tar::{self, Archive};

const USER_AGENT: &str = "crates-admin";

#[derive(clap::Parser, Debug)]
#[command(
    name = "render-readmes",
    about = "Iterates over every crate versions ever uploaded and (re-)renders their \
        readme using the readme renderer from the crates_io crate.",
    after_help = "Warning: this can take a lot of time."
)]
pub struct Opts {
    /// How many versions should be queried and processed at a time.
    #[arg(long, default_value = "25")]
    page_size: usize,

    /// Only rerender readmes that are older than this date.
    #[arg(long)]
    older_than: Option<String>,

    /// Only rerender readmes for the specified crate.
    #[arg(long = "crate")]
    crate_name: Option<String>,
}

pub fn run(opts: Opts) -> anyhow::Result<()> {
    let storage = Arc::new(Storage::from_environment());
    let conn = &mut db::oneoff_connection().unwrap();

    let start_time = Utc::now();

    let older_than = if let Some(ref time) = opts.older_than {
        Utc.datetime_from_str(time, "%Y-%m-%d %H:%M:%S")
            .expect("Could not parse --older-than argument as a time")
    } else {
        start_time
    };
    let older_than = older_than.naive_utc();

    println!("Start time:                   {start_time}");
    println!("Rendering readmes older than: {older_than}");

    let mut query = versions::table
        .inner_join(crates::table)
        .left_outer_join(readme_renderings::table)
        .filter(
            readme_renderings::rendered_at
                .lt(older_than)
                .or(readme_renderings::version_id.is_null()),
        )
        .select(versions::id)
        .into_boxed();

    if let Some(crate_name) = opts.crate_name {
        println!("Rendering readmes for {crate_name}");
        query = query.filter(crates::name.eq(crate_name));
    }

    let version_ids: Vec<i32> = query.load(conn).expect("error loading version ids");

    let total_versions = version_ids.len();
    println!("Rendering {total_versions} versions");

    let page_size = opts.page_size;

    let total_pages = total_versions / page_size;
    let total_pages = if total_versions % page_size == 0 {
        total_pages
    } else {
        total_pages + 1
    };

    let client = Client::new();

    for (page_num, version_ids_chunk) in version_ids.chunks(page_size).enumerate() {
        println!(
            "= Page {} of {} ==================================",
            page_num + 1,
            total_pages
        );

        let versions: Vec<(Version, String)> = versions::table
            .inner_join(crates::table)
            .filter(versions::id.eq_any(version_ids_chunk))
            .select((versions::all_columns, crates::name))
            .load(conn)
            .expect("error loading versions");

        let mut tasks = Vec::with_capacity(page_size);
        for (version, krate_name) in versions {
            Version::record_readme_rendering(version.id, conn)
                .context("Couldn't record rendering time")?;

            let client = client.clone();
            let storage = storage.clone();
            let handle = thread::spawn::<_, anyhow::Result<()>>(move || {
                println!("[{}-{}] Rendering README...", krate_name, version.num);
                let readme = get_readme(&storage, &client, &version, &krate_name)?;
                if !readme.is_empty() {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .context("Failed to initialize tokio runtime")
                        .unwrap();

                    rt.block_on(storage.upload_readme(&krate_name, &version.num, readme.into()))
                        .context("Failed to upload rendered README file to S3")?;
                }

                Ok(())
            });
            tasks.push(handle);
        }
        for handle in tasks {
            match handle.join() {
                Err(err) => println!("Thread panicked: {err:?}"),
                Ok(Err(err)) => println!("Thread failed: {err:?}"),
                _ => {}
            }
        }
    }

    Ok(())
}

/// Renders the readme of an uploaded crate version.
fn get_readme(
    storage: &Storage,
    client: &Client,
    version: &Version,
    krate_name: &str,
) -> anyhow::Result<String> {
    let pkg_name = format!("{}-{}", krate_name, version.num);

    let location = storage.crate_location(krate_name, &version.num.to_string());

    let mut extra_headers = header::HeaderMap::new();
    extra_headers.insert(
        header::USER_AGENT,
        header::HeaderValue::from_static(USER_AGENT),
    );
    let request = client.get(location).headers(extra_headers);
    let response = request.send().context("Failed to fetch crate")?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Failed to get a 200 response: {}",
            response.text().unwrap()
        ));
    }

    let reader = GzDecoder::new(response);
    let archive = Archive::new(reader);
    render_pkg_readme(archive, &pkg_name)
}

fn render_pkg_readme<R: Read>(mut archive: Archive<R>, pkg_name: &str) -> anyhow::Result<String> {
    let mut entries = archive.entries().context("Invalid tar archive entries")?;

    let manifest: Manifest = {
        let path = format!("{pkg_name}/Cargo.toml");
        let contents = find_file_by_path(&mut entries, Path::new(&path))
            .context("Failed to read Cargo.toml file")?;

        Manifest::from_str(&contents).context("Failed to parse manifest file")?

        // We don't call `validate_manifest()` here since the additional validation is not needed
        // and it would prevent us from reading a couple of legacy crate files.
    };

    let rendered = {
        let readme = manifest.package().readme();
        if !readme.is_some() {
            return Ok("".to_string());
        }

        let readme_path = readme.as_path().unwrap_or_else(|| Path::new("README.md"));
        let path = Path::new(pkg_name).join(readme_path);
        let contents = find_file_by_path(&mut entries, Path::new(&path))
            .with_context(|| format!("Failed to read {} file", readme_path.display()))?;

        // pkg_path_in_vcs Unsupported from admin::render_readmes. See #4095
        // Would need access to cargo_vcs_info
        let pkg_path_in_vcs = None;

        text_to_html(
            &contents,
            &readme_path,
            manifest.package().repository(),
            pkg_path_in_vcs,
        )
    };
    Ok(rendered)
}

/// Search an entry by its path in a Tar archive.
fn find_file_by_path<R: Read>(
    entries: &mut tar::Entries<'_, R>,
    path: &Path,
) -> anyhow::Result<String> {
    let mut file = entries
        .filter_map(|entry| entry.ok())
        .find(|file| match file.path() {
            Ok(p) => p == path,
            Err(_) => false,
        })
        .ok_or_else(|| anyhow!("Failed to find tarball entry: {}", path.display()))?;

    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .context("Failed to read file contents")?;

    Ok(contents)
}

#[cfg(test)]
pub mod tests {
    use crates_io_tarball::TarballBuilder;

    use super::render_pkg_readme;

    #[test]
    fn test_render_pkg_readme() {
        let serialized_archive = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
name = "foo"
version = "0.0.1"
readme = "README.md"
"#,
            )
            .add_file("foo-0.0.1/README.md", b"readme")
            .build_unzipped();

        let result =
            render_pkg_readme(tar::Archive::new(&*serialized_archive), "foo-0.0.1").unwrap();
        assert!(result.contains("readme"))
    }

    #[test]
    fn test_render_pkg_no_readme() {
        let serialized_archive = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
"#,
            )
            .build_unzipped();

        assert_err!(render_pkg_readme(
            tar::Archive::new(&*serialized_archive),
            "foo-0.0.1"
        ));
    }

    #[test]
    fn test_render_pkg_implicit_readme() {
        let serialized_archive = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
name = "foo"
version = "0.0.1"
"#,
            )
            .add_file("foo-0.0.1/README.md", b"readme")
            .build_unzipped();

        let result =
            render_pkg_readme(tar::Archive::new(&*serialized_archive), "foo-0.0.1").unwrap();
        assert!(result.contains("readme"))
    }

    #[test]
    fn test_render_pkg_readme_w_link() {
        let serialized_archive = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
name = "foo"
version = "0.0.1"
readme = "README.md"
repository = "https://github.com/foo/foo"
"#,
            )
            .add_file("foo-0.0.1/README.md", b"readme [link](./Other.md)")
            .build_unzipped();

        let result =
            render_pkg_readme(tar::Archive::new(&*serialized_archive), "foo-0.0.1").unwrap();
        assert!(result.contains("\"https://github.com/foo/foo/blob/HEAD/./Other.md\""))
    }

    #[test]
    fn test_render_pkg_readme_not_at_root() {
        let serialized_archive = TarballBuilder::new("foo", "0.0.1")
            .add_raw_manifest(
                br#"
[package]
name = "foo"
version = "0.0.1"
readme = "docs/README.md"
repository = "https://github.com/foo/foo"
"#,
            )
            .add_file(
                "foo-0.0.1/docs/README.md",
                b"docs/readme [link](./Other.md)",
            )
            .build_unzipped();

        let result =
            render_pkg_readme(tar::Archive::new(&*serialized_archive), "foo-0.0.1").unwrap();
        assert!(result.contains("docs/readme"));
        assert!(result.contains("\"https://github.com/foo/foo/blob/HEAD/docs/./Other.md\""))
    }
}
