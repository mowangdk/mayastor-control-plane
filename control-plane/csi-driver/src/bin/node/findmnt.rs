use crate::{error::DeviceError, filesystem_ops::FileSystem};
use csi_driver::filesystem::FileSystem as Fs;
use utils::csi_plugin_name;

use serde_json::Value;
use std::{
    borrow::Cow,
    collections::HashMap,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    str::FromStr,
    string::String,
    vec::Vec,
};
use tracing::{error, warn};

// Keys of interest we expect to find in the JSON output generated
// by findmnt.
const TARGET_KEY: &str = "target";
const SOURCE_KEY: &str = "source";
const FSTYPE_KEY: &str = "fstype";

#[derive(Debug)]
pub(crate) struct DeviceMount {
    mount_path: String,
    fstype: FileSystem,
}

impl std::fmt::Display for DeviceMount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MountPath: {}, FileSystem: {}",
            self.mount_path, self.fstype
        )
    }
}

impl DeviceMount {
    /// create new DeviceMount
    pub(crate) fn new(mount_path: String, fstype: FileSystem) -> DeviceMount {
        Self { mount_path, fstype }
    }
    /// File system type
    pub(crate) fn fstype(&self) -> FileSystem {
        self.fstype.clone()
    }
    /// Get the mount path.
    pub(crate) fn mount_path(&self) -> String {
        self.mount_path.clone()
    }
}

#[derive(Debug)]
struct Filter<'a> {
    key: &'a str,
    value: &'a str,
}

/// Convert a json value of a key-value pair, to a string,
/// adjusted if required, on the key.
///
/// The source field returned from findmnt can be different for
/// the same source on different systems, for example
///   dev[/nvme0n1], udev[/nvme0n1], tmpfs[/nvme0n1], devtmpfs[/nvme0n1]
/// this function converts those values to the expected /dev/nvme0n1
fn key_adjusted_value(key: &str, value: &Value) -> String {
    lazy_static::lazy_static! {
        static ref RE_UDEVPATH: regex::Regex = regex::Regex::new(
            r"(?x).*\[(?P<device>/.*)\]
        ",
        )
        .unwrap();
    }

    // Do NOT do
    //    let strvalue = value.to_string();
    // that will return a string delimited with quote characters.
    let strvalue: String = match value {
        Value::String(str) => str.to_string(),
        _ => value.to_string(),
    };
    if key == SOURCE_KEY {
        if let Some(caps) = RE_UDEVPATH.captures(&strvalue) {
            return format!("/dev{}", &caps["device"]);
        };
    }
    strvalue
}

const KEYS: &[&str] = &[TARGET_KEY, SOURCE_KEY, FSTYPE_KEY];

/// Convert the json map entry to a hashmap of strings
/// with source, target and fstype key-value pairs only.
/// The source field returned from findmnt is converted
/// to the /dev/xxx form if required.
fn jsonmap_to_hashmap(json_map: &serde_json::Map<String, Value>) -> HashMap<String, String> {
    let mut hmap: HashMap<String, String> = HashMap::new();
    for (key, value) in json_map {
        if KEYS.contains(&key.as_str()) {
            hmap.insert(key.clone(), key_adjusted_value(key, value));
        }
    }
    hmap
}

/// This function recurses over the de-serialised JSON returned by findmnt,
/// finding entries which have key-value pair's matching the
/// filter key-value pair, and populates a vector with those
/// entries as hashmaps of strings.
///
/// The assumptions made on the structure are:
///  1. An object has keys named "target" and "source" for a mount point.
///  2. An object may contain nested arrays of objects.
///
/// The search is deliberately generic (and hence slower) in an attempt to
/// be more robust to future changes in findmnt.
fn filter_findmnt(
    json_val: &serde_json::value::Value,
    filter: &Filter,
    results: &mut Vec<HashMap<String, String>>,
) {
    match json_val {
        Value::Array(json_array) => {
            for jsonvalue in json_array {
                filter_findmnt(jsonvalue, filter, results);
            }
        }
        Value::Object(json_map) => {
            if let Some(value) = json_map.get(filter.key) {
                if filter.value == value || filter.value == key_adjusted_value(filter.key, value) {
                    results.push(jsonmap_to_hashmap(json_map));
                }
            }
            // If the object has arrays, then the assumption is that they are
            // arrays of objects.
            for (_, jsonvalue) in json_map {
                if jsonvalue.is_array() {
                    filter_findmnt(jsonvalue, filter, results);
                }
            }
        }
        jvalue => {
            warn!("Unexpected json type {}", jvalue);
        }
    };
}

/// findmnt executable name.
const FIND_MNT: &str = "findmnt";
/// findmnt arguments, we only want source, target and filesystem type fields.
const FIND_MNT_ARGS: [&str; 3] = ["-J", "-o", "SOURCE,TARGET,FSTYPE"];

/// Execute the Linux utility findmnt, collect the json output,
/// invoke the filter function and return the filtered results.
async fn findmnt(params: Filter<'_>) -> Result<Vec<HashMap<String, String>>, DeviceError> {
    let output = tokio::process::Command::new(FIND_MNT)
        .args(FIND_MNT_ARGS)
        .output()
        .await?;

    if output.status.success() {
        let json_str = String::from_utf8(output.stdout)?;
        let json: Value = serde_json::from_str(&json_str)?;
        let mut results: Vec<HashMap<String, String>> = Vec::new();
        filter_findmnt(&json, &params, &mut results);
        Ok(results)
    } else {
        Err(DeviceError::new(String::from_utf8(output.stderr)?.as_str()))
    }
}

/// Use the Linux utility findmnt to find the name of the device mounted at a
/// directory or block special file, if any.
/// mount_path is the path a device is mounted on.
pub(crate) async fn get_devicepath(mount_path: &str) -> Result<Option<String>, DeviceError> {
    let tgt_filter = Filter {
        key: TARGET_KEY,
        value: mount_path,
    };
    let sources = findmnt(tgt_filter).await?;
    {
        match sources.len() {
            0 => Ok(None),
            1 => {
                if let Some(devicepath) = sources[0].get(SOURCE_KEY) {
                    Ok(Some(devicepath.to_string()))
                } else {
                    Err(DeviceError::new("missing source field"))
                }
            }
            _ => {
                // should be impossible ...
                warn!(
                    "multiple sources mounted on target {:?}->{}",
                    sources, mount_path
                );
                Err(DeviceError::new(
                    format!("multiple devices mounted at {mount_path}").as_str(),
                ))
            }
        }
    }
}

/// Use the Linux utility findmnt to find the mount paths for a block device,
/// if any.
/// device_path is the path to the device for example "/dev/sda1"
pub(crate) async fn get_mountpaths(device_path: &str) -> Result<Vec<DeviceMount>, DeviceError> {
    let dev_filter = Filter {
        key: SOURCE_KEY,
        value: device_path,
    };
    match findmnt(dev_filter).await {
        Ok(results) => {
            let mut mountpaths: Vec<DeviceMount> = Vec::new();
            for entry in results {
                if let Some(mountpath) = entry.get(TARGET_KEY) {
                    if let Some(fstype) = entry.get(FSTYPE_KEY) {
                        mountpaths.push(DeviceMount::new(
                            mountpath.to_string(),
                            Fs::from_str(fstype)
                                .unwrap_or(Fs::Unsupported(fstype.to_string()))
                                .into(),
                        ))
                    } else {
                        error!("Missing fstype for {}", mountpath);
                        mountpaths.push(DeviceMount::new(
                            mountpath.to_string(),
                            Fs::Unsupported("".to_string()).into(),
                        ))
                    }
                } else {
                    warn!("missing target field {:?}", entry);
                }
            }
            Ok(mountpaths)
        }
        Err(e) => Err(e),
    }
}

const DRIVER_NAME: &str = "driverName";
const VOLUME_HANDLE: &str = "volumeHandle";
const METADATA_FILE: &str = "vol_data.json";
const DEVICE_PATTERN: &str = "nvme";

fn block_device_name<'a>(fstype: &str, root: &'a str, allow_deleted: bool) -> Option<&'a str> {
    if !matches!(fstype, "devtmpfs" | "tmpfs") {
        return None;
    }
    let root = if allow_deleted {
        root.strip_suffix("//deleted").unwrap_or(root)
    } else {
        root
    };
    root.strip_prefix('/')
        .filter(|name| !name.is_empty() && !name.contains('/'))
}

pub(crate) enum MountQuery {
    Device(String),
    Csi { volume_id: String, kubelet: PathBuf },
}

/// Read a consistent snapshot, as in devinfo's workaround for older kernels.
pub(crate) async fn force_unstage_mountpaths(
    query: MountQuery,
) -> Result<Vec<DeviceMount>, DeviceError> {
    super::runtime::spawn_blocking(move || {
        const MOUNTINFO: &str = "/proc/self/mountinfo";
        let mut previous = std::fs::read_to_string(MOUNTINFO)?;
        for _ in 0..=10 {
            let current = std::fs::read_to_string(MOUNTINFO)?;
            if previous == current {
                return scan_mounts(&current, query);
            }
            previous = current;
        }
        Err(DeviceError::new(
            "Could not obtain a consistent mountinfo snapshot",
        ))
    })
    .await
    .map_err(|err| DeviceError::new(err.to_string()))?
}

// procfs-core exposes the kernel-escaped strings in its mountinfo records.
fn decode_mountinfo_path(path: &str) -> Result<Cow<'_, str>, DeviceError> {
    let Some((start, escaped)) = path.split_once('\\') else {
        return Ok(Cow::Borrowed(path));
    };

    let invalid = || DeviceError::new(format!("Invalid mountinfo path: {path}"));
    let mut decoded = Vec::with_capacity(path.len());
    decoded.extend_from_slice(start.as_bytes());
    for part in escaped.split('\\') {
        let byte = match part.as_bytes() {
            [a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', ..] => {
                (a - b'0') * 64 + (b - b'0') * 8 + (c - b'0')
            }
            _ => return Err(invalid()),
        };
        decoded.push(byte);
        decoded.extend_from_slice(&part.as_bytes()[3..]);
    }
    String::from_utf8(decoded)
        .map(Cow::Owned)
        .map_err(|_| invalid())
}

fn scan_mounts(mounts: &str, mut query: MountQuery) -> Result<Vec<DeviceMount>, DeviceError> {
    if let MountQuery::Csi { kubelet, .. } = &mut query {
        *kubelet = kubelet.canonicalize()?;
    }
    let driver = csi_plugin_name();
    let mut paths = Vec::new();
    for line in mounts.lines() {
        let mount = procfs_core::process::MountInfo::from_line(line)
            .map_err(|error| DeviceError::new(error.to_string()))?;
        let target =
            decode_mountinfo_path(mount.mount_point.to_str().expect("parsed from a string"))?;
        let target = Path::new(target.as_ref());
        let root = decode_mountinfo_path(&mount.root)?;
        let source = decode_mountinfo_path(mount.mount_source.as_deref().unwrap_or_default())?;
        let block = block_device_name(
            &mount.fs_type,
            &root,
            matches!(query, MountQuery::Csi { .. }),
        );
        let matches = match &query {
            MountQuery::Device(device) => {
                source == device.as_str()
                    || block.is_some_and(|name| device.strip_prefix("/dev/") == Some(name))
            }
            MountQuery::Csi { volume_id, kubelet } => {
                block.unwrap_or(&source).contains(DEVICE_PATTERN)
                    && target.starts_with(kubelet)
                    && csi_mount_metadata_path(target, kubelet, block.is_some())
                        .is_some_and(|path| csi_volume_matches(&path, &driver, volume_id))
            }
        };
        if matches
            && (block.is_none()
                || std::fs::symlink_metadata(target)?
                    .file_type()
                    .is_block_device())
        {
            let fstype = Fs::from_str(&mount.fs_type).unwrap_or(Fs::Unsupported(mount.fs_type));
            paths.push(DeviceMount::new(
                target.to_string_lossy().into_owned(),
                fstype.into(),
            ));
        }
    }
    Ok(paths)
}

fn csi_mount_metadata_path(target: &Path, kubelet_path: &Path, block: bool) -> Option<PathBuf> {
    if block {
        let devices = kubelet_path.join("plugins/kubernetes.io/csi/volumeDevices");
        if let Ok(relative) = target.strip_prefix(&devices) {
            let parts: Vec<_> = relative.iter().collect();
            let volume = match parts.as_slice() {
                [publish, volume, _] if *publish == "publish" => volume,
                [volume, dev, _] if *dev == "dev" => volume,
                _ => return None,
            };
            return Some(devices.join(volume).join("data").join(METADATA_FILE));
        }
    }
    target.parent().map(|parent| parent.join(METADATA_FILE))
}

fn csi_volume_matches(path: &Path, driver: &str, volume_id: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let reader = std::io::BufReader::new(file);
    let Ok(data) = serde_json::from_reader::<_, Value>(reader) else {
        return false;
    };
    data.get(DRIVER_NAME).and_then(Value::as_str) == Some(driver)
        && data.get(VOLUME_HANDLE).and_then(Value::as_str) == Some(volume_id)
}

#[cfg(test)]
mod mountinfo_tests {
    use super::*;

    #[test]
    fn scans_valid_mounts_and_propagates_parse_errors() {
        let mounts = concat!(
            "1 0 0:1 / /empty rw - tmpfs  rw\n",
            r"2 0 0:2 / /hash rw - tmpfs source\043name rw",
            "\n",
            "3 0 0:3 /nvme0n1//deleted /old rw - devtmpfs udev rw\n",
            r"4 0 259:0 / /live\043mount rw - ext4 /dev/nvme0n1 rw",
        );
        let device = || MountQuery::Device("/dev/nvme0n1".into());
        let paths = scan_mounts(mounts, device()).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].mount_path(), "/live#mount");
        assert_eq!(
            block_device_name("devtmpfs", "/nvme0n1//deleted", true),
            Some("nvme0n1")
        );
        assert!(scan_mounts(&format!("{mounts}\ninvalid mountinfo"), device()).is_err());
    }

    #[test]
    fn finds_csi_metadata_through_symlinked_kubelet_root() -> std::io::Result<()> {
        let root = std::env::temp_dir().join(format!("mountinfo-{}", uuid::Uuid::new_v4()));
        let kubelet = root.join("kubelet");
        let mount_parent = kubelet.join("volume");
        std::fs::create_dir_all(&mount_parent)?;
        let link = root.join("link");
        std::os::unix::fs::symlink(&kubelet, &link)?;
        std::fs::write(
            mount_parent.join(METADATA_FILE),
            serde_json::json!({"driverName": csi_plugin_name(), "volumeHandle": "volume"})
                .to_string(),
        )?;
        let target = mount_parent.canonicalize()?.join("mount");
        let mounts = format!(
            "71 30 259:0 / {} rw - ext4 /dev/nvme0n1 rw",
            target.display()
        );
        let query = |id: &str| MountQuery::Csi {
            volume_id: id.into(),
            kubelet: link.clone(),
        };
        let paths = scan_mounts(&mounts, query("volume")).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(Path::new(&paths[0].mount_path()), target);
        assert!(scan_mounts(&mounts, query("other-volume"))
            .unwrap()
            .is_empty());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn locates_raw_block_metadata_for_both_kubelet_mounts() {
        let kubelet = Path::new("/var/lib/kubelet");
        let devices = kubelet.join("plugins/kubernetes.io/csi/volumeDevices");
        let expected = devices.join("pvc-volume/data/vol_data.json");
        for target in ["publish/pvc-volume/pod", "pvc-volume/dev/pod"] {
            assert_eq!(
                csi_mount_metadata_path(&devices.join(target), kubelet, true).unwrap(),
                expected
            );
        }
    }
}
