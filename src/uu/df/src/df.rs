// This file is part of the uutils coreutils package.
//
// (c) Fangxu Hu <framlog@gmail.com>
// (c) Sylvestre Ledru <sylvestre@debian.org>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.
// spell-checker:ignore itotal iused iavail ipcent pcent tmpfs squashfs
mod blocks;
mod table;

use uucore::error::{UResult, USimpleError};
use uucore::format_usage;
#[cfg(unix)]
use uucore::fsext::statfs;
use uucore::fsext::{read_fs_list, FsUsage, MountInfo};

use clap::{crate_version, App, AppSettings, Arg, ArgMatches};

use std::collections::HashSet;
use std::fmt;
use std::iter::FromIterator;

#[cfg(windows)]
use std::path::Path;

use crate::blocks::{block_size_from_matches, BlockSize};
use crate::table::{DisplayRow, Header, Row};

static ABOUT: &str = "Show information about the file system on which each FILE resides,\n\
                      or all file systems by default.";
const USAGE: &str = "{} [OPTION]... [FILE]...";

static OPT_ALL: &str = "all";
static OPT_BLOCKSIZE: &str = "blocksize";
static OPT_DIRECT: &str = "direct";
static OPT_TOTAL: &str = "total";
static OPT_HUMAN_READABLE: &str = "human-readable";
static OPT_HUMAN_READABLE_2: &str = "human-readable-2";
static OPT_INODES: &str = "inodes";
static OPT_KILO: &str = "kilo";
static OPT_LOCAL: &str = "local";
static OPT_NO_SYNC: &str = "no-sync";
static OPT_OUTPUT: &str = "output";
static OPT_PATHS: &str = "paths";
static OPT_PORTABILITY: &str = "portability";
static OPT_SYNC: &str = "sync";
static OPT_TYPE: &str = "type";
static OPT_PRINT_TYPE: &str = "print-type";
static OPT_EXCLUDE_TYPE: &str = "exclude-type";
static OUTPUT_FIELD_LIST: [&str; 12] = [
    "source", "fstype", "itotal", "iused", "iavail", "ipcent", "size", "used", "avail", "pcent",
    "file", "target",
];

/// Store names of file systems as a selector.
/// Note: `exclude` takes priority over `include`.
#[derive(Default)]
struct FsSelector {
    include: HashSet<String>,
    exclude: HashSet<String>,
}

/// Parameters that control the behavior of `df`.
///
/// Most of these parameters control which rows and which columns are
/// displayed. The `block_size` determines the units to use when
/// displaying numbers of bytes or inodes.
#[derive(Default)]
struct Options {
    show_local_fs: bool,
    show_all_fs: bool,
    show_listed_fs: bool,
    show_fs_type: bool,
    show_inode_instead: bool,
    block_size: BlockSize,
    fs_selector: FsSelector,

    /// Whether to show a final row comprising the totals for each column.
    show_total: bool,
}

enum OptionsError {
    InvalidBlockSize,
}

impl fmt::Display for OptionsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            // TODO This should include the raw string provided as the argument.
            //
            // TODO This needs to vary based on whether `--block-size`
            // or `-B` were provided.
            Self::InvalidBlockSize => write!(f, "invalid --block-size argument"),
        }
    }
}

impl Options {
    /// Convert command-line arguments into [`Options`].
    fn from(matches: &ArgMatches) -> Result<Self, OptionsError> {
        Ok(Self {
            show_local_fs: matches.is_present(OPT_LOCAL),
            show_all_fs: matches.is_present(OPT_ALL),
            show_listed_fs: false,
            show_fs_type: matches.is_present(OPT_PRINT_TYPE),
            show_inode_instead: matches.is_present(OPT_INODES),
            block_size: block_size_from_matches(matches)
                .map_err(|_| OptionsError::InvalidBlockSize)?,
            fs_selector: FsSelector::from(matches),
            show_total: matches.is_present(OPT_TOTAL),
        })
    }
}

#[derive(Debug, Clone)]
struct Filesystem {
    mount_info: MountInfo,
    usage: FsUsage,
}

impl FsSelector {
    /// Convert command-line arguments into a [`FsSelector`].
    ///
    /// This function reads the include and exclude sets from
    /// [`ArgMatches`] and returns the corresponding [`FsSelector`]
    /// instance.
    fn from(matches: &ArgMatches) -> Self {
        let include = HashSet::from_iter(matches.values_of_lossy(OPT_TYPE).unwrap_or_default());
        let exclude = HashSet::from_iter(
            matches
                .values_of_lossy(OPT_EXCLUDE_TYPE)
                .unwrap_or_default(),
        );
        Self { include, exclude }
    }

    fn should_select(&self, fs_type: &str) -> bool {
        if self.exclude.contains(fs_type) {
            return false;
        }
        self.include.is_empty() || self.include.contains(fs_type)
    }
}

impl Filesystem {
    // TODO: resolve uuid in `mount_info.dev_name` if exists
    fn new(mount_info: MountInfo) -> Option<Self> {
        let _stat_path = if !mount_info.mount_dir.is_empty() {
            mount_info.mount_dir.clone()
        } else {
            #[cfg(unix)]
            {
                mount_info.dev_name.clone()
            }
            #[cfg(windows)]
            {
                // On windows, we expect the volume id
                mount_info.dev_id.clone()
            }
        };
        #[cfg(unix)]
        let usage = FsUsage::new(statfs(_stat_path).ok()?);
        #[cfg(windows)]
        let usage = FsUsage::new(Path::new(&_stat_path));
        Some(Self { mount_info, usage })
    }
}

/// Whether to display the mount info given the inclusion settings.
fn is_included(mi: &MountInfo, paths: &[String], opt: &Options) -> bool {
    // Don't show remote filesystems if `--local` has been given.
    if mi.remote && opt.show_local_fs {
        return false;
    }

    // Don't show pseudo filesystems unless `--all` has been given.
    if mi.dummy && !opt.show_all_fs && !opt.show_listed_fs {
        return false;
    }

    // Don't show filesystems if they have been explicitly excluded.
    if !opt.fs_selector.should_select(&mi.fs_type) {
        return false;
    }

    // Don't show filesystems other than the ones specified on the
    // command line, if any.
    if !paths.is_empty() && !paths.contains(&mi.mount_dir) {
        return false;
    }

    true
}

/// Whether the mount info in `m2` should be prioritized over `m1`.
///
/// The "lt" in the function name is in analogy to the
/// [`std::cmp::PartialOrd::lt`].
fn mount_info_lt(m1: &MountInfo, m2: &MountInfo) -> bool {
    // let "real" devices with '/' in the name win.
    if m1.dev_name.starts_with('/') && !m2.dev_name.starts_with('/') {
        return false;
    }

    let m1_nearer_root = m1.mount_dir.len() < m2.mount_dir.len();
    // With bind mounts, prefer items nearer the root of the source
    let m2_below_root = !m1.mount_root.is_empty()
        && !m2.mount_root.is_empty()
        && m1.mount_root.len() > m2.mount_root.len();
    // let points towards the root of the device win.
    if m1_nearer_root && !m2_below_root {
        return false;
    }

    // let an entry over-mounted on a new device win, but only when
    // matching an existing mnt point, to avoid problematic
    // replacement when given inaccurate mount lists, seen with some
    // chroot environments for example.
    if m1.dev_name != m2.dev_name && m1.mount_dir == m2.mount_dir {
        return false;
    }

    true
}

/// Whether to prioritize given mount info over all others on the same device.
///
/// This function decides whether the mount info `mi` is better than
/// all others in `previous` that mount the same device as `mi`.
fn is_best(previous: &[MountInfo], mi: &MountInfo) -> bool {
    for seen in previous {
        if seen.dev_id == mi.dev_id && mount_info_lt(mi, seen) {
            return false;
        }
    }
    true
}

/// Keep only the specified subset of [`MountInfo`] instances.
///
/// If `paths` is non-empty, this function excludes any [`MountInfo`]
/// that is not mounted at the specified path.
///
/// The `opt` argument specifies a variety of ways of excluding
/// [`MountInfo`] instances; see [`Options`] for more information.
///
/// Finally, if there are duplicate entries, the one with the shorter
/// path is kept.
fn filter_mount_list(vmi: Vec<MountInfo>, paths: &[String], opt: &Options) -> Vec<MountInfo> {
    let mut result = vec![];
    for mi in vmi {
        // TODO The running time of the `is_best()` function is linear
        // in the length of `result`. That makes the running time of
        // this loop quadratic in the length of `vmi`. This could be
        // improved by a more efficient implementation of `is_best()`,
        // but `vmi` is probably not very long in practice.
        if is_included(&mi, paths, opt) && is_best(&result, &mi) {
            result.push(mi);
        }
    }
    result
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches = uu_app().get_matches_from(args);

    let paths: Vec<String> = matches
        .values_of(OPT_PATHS)
        .map(|v| v.map(ToString::to_string).collect())
        .unwrap_or_default();

    #[cfg(windows)]
    {
        if matches.is_present(OPT_INODES) {
            println!("{}: doesn't support -i option", uucore::util_name());
            return Ok(());
        }
    }

    let opt = Options::from(&matches).map_err(|e| USimpleError::new(1, format!("{}", e)))?;

    let mounts = read_fs_list();
    let data: Vec<Row> = filter_mount_list(mounts, &paths, &opt)
        .into_iter()
        .filter_map(Filesystem::new)
        .filter(|fs| fs.usage.blocks != 0 || opt.show_all_fs || opt.show_listed_fs)
        .map(Into::into)
        .collect();

    println!("{}", Header::new(&opt));
    let mut total = Row::new("total");
    for row in data {
        println!("{}", DisplayRow::new(&row, &opt));
        total += row;
    }
    if opt.show_total {
        println!("{}", DisplayRow::new(&total, &opt));
    }

    Ok(())
}

pub fn uu_app<'a>() -> App<'a> {
    App::new(uucore::util_name())
        .version(crate_version!())
        .about(ABOUT)
        .override_usage(format_usage(USAGE))
        .setting(AppSettings::InferLongArgs)
        .arg(
            Arg::new(OPT_ALL)
                .short('a')
                .long("all")
                .help("include dummy file systems"),
        )
        .arg(
            Arg::new(OPT_BLOCKSIZE)
                .short('B')
                .long("block-size")
                .takes_value(true)
                .help(
                    "scale sizes by SIZE before printing them; e.g.\
                     '-BM' prints sizes in units of 1,048,576 bytes",
                ),
        )
        .arg(
            Arg::new(OPT_DIRECT)
                .long("direct")
                .help("show statistics for a file instead of mount point"),
        )
        .arg(
            Arg::new(OPT_TOTAL)
                .long("total")
                .help("produce a grand total"),
        )
        .arg(
            Arg::new(OPT_HUMAN_READABLE)
                .short('h')
                .long("human-readable")
                .conflicts_with(OPT_HUMAN_READABLE_2)
                .help("print sizes in human readable format (e.g., 1K 234M 2G)"),
        )
        .arg(
            Arg::new(OPT_HUMAN_READABLE_2)
                .short('H')
                .long("si")
                .conflicts_with(OPT_HUMAN_READABLE)
                .help("likewise, but use powers of 1000 not 1024"),
        )
        .arg(
            Arg::new(OPT_INODES)
                .short('i')
                .long("inodes")
                .help("list inode information instead of block usage"),
        )
        .arg(Arg::new(OPT_KILO).short('k').help("like --block-size=1K"))
        .arg(
            Arg::new(OPT_LOCAL)
                .short('l')
                .long("local")
                .help("limit listing to local file systems"),
        )
        .arg(
            Arg::new(OPT_NO_SYNC)
                .long("no-sync")
                .conflicts_with(OPT_SYNC)
                .help("do not invoke sync before getting usage info (default)"),
        )
        .arg(
            Arg::new(OPT_OUTPUT)
                .long("output")
                .takes_value(true)
                .use_delimiter(true)
                .possible_values(OUTPUT_FIELD_LIST)
                .default_missing_values(&OUTPUT_FIELD_LIST)
                .default_values(&["source", "size", "used", "avail", "pcent", "target"])
                .conflicts_with_all(&[OPT_INODES, OPT_PORTABILITY, OPT_PRINT_TYPE])
                .help(
                    "use the output format defined by FIELD_LIST,\
                     or print all fields if FIELD_LIST is omitted.",
                ),
        )
        .arg(
            Arg::new(OPT_PORTABILITY)
                .short('P')
                .long("portability")
                .help("use the POSIX output format"),
        )
        .arg(
            Arg::new(OPT_SYNC)
                .long("sync")
                .conflicts_with(OPT_NO_SYNC)
                .help("invoke sync before getting usage info"),
        )
        .arg(
            Arg::new(OPT_TYPE)
                .short('t')
                .long("type")
                .allow_invalid_utf8(true)
                .takes_value(true)
                .multiple_occurrences(true)
                .help("limit listing to file systems of type TYPE"),
        )
        .arg(
            Arg::new(OPT_PRINT_TYPE)
                .short('T')
                .long("print-type")
                .help("print file system type"),
        )
        .arg(
            Arg::new(OPT_EXCLUDE_TYPE)
                .short('x')
                .long("exclude-type")
                .takes_value(true)
                .use_delimiter(true)
                .help("limit listing to file systems not of type TYPE"),
        )
        .arg(Arg::new(OPT_PATHS).multiple_occurrences(true))
}

#[cfg(test)]
mod tests {

    mod mount_info_lt {

        use crate::mount_info_lt;
        use uucore::fsext::MountInfo;

        /// Instantiate a [`MountInfo`] with the given fields.
        fn mount_info(dev_name: &str, mount_root: &str, mount_dir: &str) -> MountInfo {
            MountInfo {
                dev_id: String::new(),
                dev_name: String::from(dev_name),
                fs_type: String::new(),
                mount_dir: String::from(mount_dir),
                mount_option: String::new(),
                mount_root: String::from(mount_root),
                remote: false,
                dummy: false,
            }
        }

        #[test]
        fn test_absolute() {
            // Prefer device name "/dev/foo" over "dev_foo".
            let m1 = mount_info("/dev/foo", "/", "/mnt/bar");
            let m2 = mount_info("dev_foo", "/", "/mnt/bar");
            assert!(!mount_info_lt(&m1, &m2));
        }

        #[test]
        fn test_shorter() {
            // Prefer mount directory "/mnt/bar" over "/mnt/bar/baz"...
            let m1 = mount_info("/dev/foo", "/", "/mnt/bar");
            let m2 = mount_info("/dev/foo", "/", "/mnt/bar/baz");
            assert!(!mount_info_lt(&m1, &m2));

            // ..but prefer mount root "/root" over "/".
            let m1 = mount_info("/dev/foo", "/root", "/mnt/bar");
            let m2 = mount_info("/dev/foo", "/", "/mnt/bar/baz");
            assert!(mount_info_lt(&m1, &m2));
        }

        #[test]
        fn test_over_mounted() {
            // Prefer the earlier entry if the devices are different but
            // the mount directory is the same.
            let m1 = mount_info("/dev/foo", "/", "/mnt/baz");
            let m2 = mount_info("/dev/bar", "/", "/mnt/baz");
            assert!(!mount_info_lt(&m1, &m2));
        }
    }

    mod is_best {

        use crate::is_best;
        use uucore::fsext::MountInfo;

        /// Instantiate a [`MountInfo`] with the given fields.
        fn mount_info(dev_id: &str, mount_dir: &str) -> MountInfo {
            MountInfo {
                dev_id: String::from(dev_id),
                dev_name: String::new(),
                fs_type: String::new(),
                mount_dir: String::from(mount_dir),
                mount_option: String::new(),
                mount_root: String::new(),
                remote: false,
                dummy: false,
            }
        }

        #[test]
        fn test_empty() {
            let m = mount_info("0", "/mnt/bar");
            assert!(is_best(&[], &m));
        }

        #[test]
        fn test_different_dev_id() {
            let m1 = mount_info("0", "/mnt/bar");
            let m2 = mount_info("1", "/mnt/bar");
            assert!(is_best(&[m1.clone()], &m2));
            assert!(is_best(&[m2], &m1));
        }

        #[test]
        fn test_same_dev_id() {
            // There are several conditions under which a `MountInfo` is
            // considered "better" than the others, we're just checking
            // one condition in this test.
            let m1 = mount_info("0", "/mnt/bar");
            let m2 = mount_info("0", "/mnt/bar/baz");
            assert!(!is_best(&[m1.clone()], &m2));
            assert!(is_best(&[m2], &m1));
        }
    }

    mod is_included {

        use crate::{is_included, FsSelector, Options};
        use std::collections::HashSet;
        use uucore::fsext::MountInfo;

        /// Instantiate a [`MountInfo`] with the given fields.
        fn mount_info(fs_type: &str, mount_dir: &str, remote: bool, dummy: bool) -> MountInfo {
            MountInfo {
                dev_id: String::new(),
                dev_name: String::new(),
                fs_type: String::from(fs_type),
                mount_dir: String::from(mount_dir),
                mount_option: String::new(),
                mount_root: String::new(),
                remote,
                dummy,
            }
        }

        #[test]
        fn test_remote_included() {
            let opt = Default::default();
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", true, false);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_remote_excluded() {
            let opt = Options {
                show_local_fs: true,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", true, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_dummy_included() {
            let opt = Options {
                show_all_fs: true,
                show_listed_fs: true,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, true);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_dummy_excluded() {
            let opt = Default::default();
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, true);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_exclude_match() {
            let exclude = HashSet::from([String::from("ext4")]);
            let fs_selector = FsSelector {
                exclude,
                ..Default::default()
            };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_exclude_no_match() {
            let exclude = HashSet::from([String::from("tmpfs")]);
            let fs_selector = FsSelector {
                exclude,
                ..Default::default()
            };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_match() {
            let include = HashSet::from([String::from("ext4")]);
            let fs_selector = FsSelector {
                include,
                ..Default::default()
            };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_no_match() {
            let include = HashSet::from([String::from("tmpfs")]);
            let fs_selector = FsSelector {
                include,
                ..Default::default()
            };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_and_exclude_match_neither() {
            let include = HashSet::from([String::from("tmpfs")]);
            let exclude = HashSet::from([String::from("squashfs")]);
            let fs_selector = FsSelector { include, exclude };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_and_exclude_match_exclude() {
            let include = HashSet::from([String::from("tmpfs")]);
            let exclude = HashSet::from([String::from("ext4")]);
            let fs_selector = FsSelector { include, exclude };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_and_exclude_match_include() {
            let include = HashSet::from([String::from("ext4")]);
            let exclude = HashSet::from([String::from("squashfs")]);
            let fs_selector = FsSelector { include, exclude };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_include_and_exclude_match_both() {
            // TODO The same filesystem type in both `include` and
            // `exclude` should cause an error, but currently does
            // not.
            let include = HashSet::from([String::from("ext4")]);
            let exclude = HashSet::from([String::from("ext4")]);
            let fs_selector = FsSelector { include, exclude };
            let opt = Options {
                fs_selector,
                ..Default::default()
            };
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_paths_empty() {
            let opt = Default::default();
            let paths = [];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_not_in_paths() {
            let opt = Default::default();
            let paths = [String::from("/mnt/foo")];
            let m = mount_info("ext4", "/mnt/bar", false, false);
            assert!(!is_included(&m, &paths, &opt));
        }

        #[test]
        fn test_in_paths() {
            let opt = Default::default();
            let paths = [String::from("/mnt/foo")];
            let m = mount_info("ext4", "/mnt/foo", false, false);
            assert!(is_included(&m, &paths, &opt));
        }
    }

    mod filter_mount_list {

        use crate::filter_mount_list;

        #[test]
        fn test_empty() {
            let opt = Default::default();
            let paths = [];
            let mount_infos = vec![];
            assert!(filter_mount_list(mount_infos, &paths, &opt).is_empty());
        }
    }
}
