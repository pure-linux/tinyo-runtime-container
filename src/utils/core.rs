// ================================================================================================
// This file contains the implementation of a lightweight container runtime using Linux namespaces
// and system calls. The runtime currently supports:
//
// - Reading container configuration from a state file (e.g., DockerHub image, port, and mounts).
// - Downloading and unpacking Docker images directly from Docker Hub.
// - Setting up a root filesystem for the container with pivot_root and custom mount points.
// - Isolating the container process with Linux namespaces (PID, NET, UTS, and mount).
// - Running a simple process inside the container.
//
// Key features of the implementation:
// 1. Direct interaction with system-level APIs using the `nix` crate.
// 2. Integration with Docker Hub for image management using `reqwest`.
// 3. High-level Rust abstractions for error handling and filesystem operations.
// 4. Performance benchmarking to measure image preparation and container startup times.
//
// Dependencies:
// - nix: System call abstractions for Unix-like systems.
// - reqwest: HTTP client for communicating with Docker Hub.
// - serde/serde_yaml: Parsing state file configurations.
// - tar: Extracting Docker image layers.
//
// Note: This is a proof-of-concept runtime and lacks advanced features like cgroups or robust error
// handling for production use.
//
// ================================================================================================

use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::wait::waitpid;
use nix::unistd::{chdir, execvp, fork, mkdir, pivot_root, sethostname, ForkResult};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_yaml;
use serde_json::Value;
use std::ffi::CString;
use std::fs::{create_dir_all, File};
use std::io::{self, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};
use tar::Archive;

// Statefile structure
#[derive(Deserialize)]
struct StateFile {
    container: String,         // Full DockerHub image name with optional tag
    port: u16,                 // Port for localhost binding
    mounts: Option<Vec<Mount>>, // Optional list of mounts
}

#[derive(Deserialize)]
struct Mount {
    source: String, // Path on the host
    target: String, // Path in the container
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let overall_start = Instant::now();

    // Path to the state file
    let state_file_path = "state.yaml";

    // Read container and port from the state file
    let state = read_state_file(state_file_path)?;
    let container_with_tag = ensure_tag(&state.container);
    let port = state.port;

    // Prepare root filesystem and measure times
    let prepare_start = Instant::now();
    let (root_fs, download_duration, unpack_duration) = prepare_root_fs(&container_with_tag)?;
    let prepare_duration = prepare_start.elapsed();

    // Start the container with direct Linux APIs
    let start_time = Instant::now();
    start_container(&root_fs, state.mounts)?;
    let start_duration = start_time.elapsed();

    // Always output the URL of the running container
    println!("Container is running at: http://localhost:{}", port);

    // Performance benchmarks
    let overall_duration = overall_start.elapsed();
    println!("\nPerformance Benchmarks:");
    println!(
        "  - Download time: {:.2} seconds",
        download_duration.as_secs_f64()
    );
    println!(
        "  - Unpack time: {:.2} seconds",
        unpack_duration.as_secs_f64()
    );
    println!(
        "  - Prepare root filesystem (total): {:.2} seconds",
        prepare_duration.as_secs_f64()
    );
    println!(
        "  - Start container: {:.2} seconds",
        start_duration.as_secs_f64()
    );
    println!(
        "  - Total time (prepare + start): {:.2} seconds",
        overall_duration.as_secs_f64()
    );

    Ok(())
}

// Reads the state file containing the Docker image name, port, and mounts
fn read_state_file(path: &str) -> Result<StateFile, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let state: StateFile = serde_yaml::from_reader(reader)?;
    Ok(state)
}

// Ensures the image name has a tag, defaults to "latest" if no tag is provided
fn ensure_tag(container: &str) -> String {
    if container.contains(':') {
        container.to_string()
    } else {
        format!("{}:latest", container)
    }
}

// Downloads and extracts a Docker image directly from Docker Hub
fn download_image(
    image: &str,
    tag: &str,
    root_fs_path: &str,
) -> Result<(Duration, Duration), Box<dyn std::error::Error>> {
    let client = Client::new();

    // Authenticate and get a token
    let token_url = format!(
        "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
        image
    );
    let token_resp: Value = client.get(&token_url).send()?.json()?;
    let token = token_resp["token"]
        .as_str()
        .or_else(|| token_resp["access_token"].as_str())
        .ok_or("Failed to get access token")?;

    // Get the manifest
    let manifest_url = format!(
        "https://registry-1.docker.io/v2/{}/manifests/{}",
        image, tag
    );
    let manifest_resp: Value = client
        .get(&manifest_url)
        .bearer_auth(token)
        .header(
            "Accept",
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .send()?
        .json()?;

    // Extract layers
    let layers = manifest_resp["layers"]
        .as_array()
        .ok_or("Failed to get layers from manifest")?;
    create_dir_all(root_fs_path)?;

    let mut total_download_duration = Duration::ZERO;
    let mut total_unpack_duration = Duration::ZERO;

    // Sequential download and extraction of layers
    for layer in layers {
        let digest = layer["digest"]
            .as_str()
            .ok_or("Failed to get layer digest")?;
        let url = format!(
            "https://registry-1.docker.io/v2/{}/blobs/{}",
            image, digest
        );

        // Download the layer
        println!("Downloading layer: {}", digest);
        let download_start = Instant::now();
        let layer_resp = client.get(&url).bearer_auth(token).send()?;
        let tar_data = layer_resp.bytes()?;
        total_download_duration += download_start.elapsed();

        // Extract the layer directly in memory
        println!("Extracting layer: {}", digest);
        let unpack_start = Instant::now();
        let mut archive = Archive::new(io::Cursor::new(tar_data));
        archive.unpack(root_fs_path)?;
        total_unpack_duration += unpack_start.elapsed();
    }

    println!("Image downloaded and extracted to {}", root_fs_path);

    Ok((total_download_duration, total_unpack_duration))
}

// Prepares the root filesystem
fn prepare_root_fs(
    image_name: &str,
) -> Result<(String, Duration, Duration), Box<dyn std::error::Error>> {
    let sanitized_image_name = image_name.replace("/", "_").replace(":", "_");
    let root_fs_path = format!("/var/lib/containers/{}", sanitized_image_name);
    if Path::new(&root_fs_path).exists() {
        return Ok((root_fs_path, Duration::ZERO, Duration::ZERO));
    }

    // Split image name and tag
    let parts: Vec<&str> = image_name.split(':').collect();
    let image = parts[0];
    let tag = if parts.len() > 1 { parts[1] } else { "latest" };

    // Download and extract image layers
    let (download_duration, unpack_duration) = download_image(image, tag, &root_fs_path)?;

    Ok((root_fs_path, download_duration, unpack_duration))
}

// Starts the container using Linux Namespaces and pivot_root with mounts
fn start_container(
    root_fs: &str,
    mounts: Option<Vec<Mount>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use nix::unistd::Uid;

    // Unshare process namespaces for isolation
    unshare(
        CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWNET
            | CloneFlags::CLONE_NEWUTS,
    )?;

    // Set hostname to something else
    sethostname("container")?;

    // Make the mount namespace private to prevent mount propagation
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )?;

    // Bind mount the container's root filesystem onto itself to make it a mount point
    mount(
        Some(root_fs),                      // Source
        root_fs,                            // Target
        None::<&str>,                       // Filesystem type
        MsFlags::MS_BIND | MsFlags::MS_REC, // Mount options
        None::<&str>,                       // Data (not used here)
    )?;

    // Handle additional mounts
    if let Some(mounts) = mounts {
        for mount_entry in mounts {
            let source_path = Path::new(&mount_entry.source);
            let target_path = Path::new(root_fs).join(&mount_entry.target);

            // Ensure the target path exists
            create_dir_all(&target_path)?;

            // Mount the source to the target
            println!(
                "Mounting {} -> {}",
                source_path.display(),
                target_path.display()
            );
            mount(
                Some(source_path),
                &target_path,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )?;
        }
    }

    // Create a directory for old root
    let old_root = Path::new(root_fs).join("old_root");
    if !old_root.exists() {
        mkdir(&old_root, nix::sys::stat::Mode::from_bits(0o755).unwrap())?;
    }

    // Change directory to the new root
    chdir(root_fs)?;

    // Perform pivot_root
    pivot_root(".", "old_root")?;

    // Change directory to the new root after pivot_root
    chdir("/")?;

    // Unmount the old root to free up space
    umount2("/old_root", MntFlags::MNT_DETACH)?;
    std::fs::remove_dir_all("/old_root")?;

    // Set up the filesystem hierarchy
    create_dir_all("/proc")?;
    create_dir_all("/sys")?;
    create_dir_all("/dev")?;

    // Mount /proc
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    // Mount /sys
    mount(
        Some("sysfs"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    // Mount /dev
    mount(
        Some("tmpfs"),
        "/dev",
        Some("tmpfs"),
        MsFlags::empty(),
        None::<&str>,
    )?;

    // Fork a new process to execute the container's command
    match fork()? {
        ForkResult::Parent { child } => {
            println!("Container started with PID: {}", child);
            waitpid(child, None)?; // Wait for the child process
        }
        ForkResult::Child => {
            // Drop privileges (optional but recommended)
            if Uid::effective() == Uid::root() {
                nix::unistd::setgid(nix::unistd::Gid::from_raw(65534))?; // nobody
                nix::unistd::setuid(nix::unistd::Uid::from_raw(65534))?;
            }

            // Execute the container's entrypoint command
            let cmd = CString::new("/bin/sh").unwrap();
            let args = [
                CString::new("sh").unwrap(), // argv[0], the program name
                CString::new("-c").unwrap(),
                CString::new(
                    "ip link set lo up && echo Hello from container! && sleep 10",
                )
                .unwrap(),
            ];
            execvp(&cmd, &args)?;
        }
    }

    Ok(())
}
