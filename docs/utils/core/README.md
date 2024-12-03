# Runtime Architecture

## Core

### Linux Libraries

The TinyO container runtime leverages several Linux libraries and features to implement containerization at a low level. Below is a list of the core components and their purposes:

The code is located here: [features/runtime/core.rs][src-features-runtime-core.rs].

- **Linux Namespaces** (`unshare`, `CloneFlags`)
Used to isolate the container's process from the host system by creating separate namespaces for PID, network, UTS (hostname), and mount points. This provides the basic building blocks for process isolation in Linux.

- **Mount API** (`mount`, `umount2`, `pivot_root`)
Responsible for setting up the container's root filesystem, making it independent of the host's file structure. `pivot_root` is crucial for replacing the current root filesystem with the container's filesystem, while `mount` and `umount2` manage additional filesystem bindings.

- **Process Control** (`fork`, `execvp`, `waitpid`)
Implements the forking mechanism to create a new containerized process and execute commands inside the container. fork creates a child process, `execvp` executes the container's entrypoint (e.g., /bin/sh), and waitpid monitors the lifecycle of the containerized process.

- **Filesystem Utilities** (`mkdir`, `chdir`)
Ensures the necessary directory structure is created for the container's filesystem. `chdir` is used to change the working directory to the container's root during the filesystem setup.

- **Hostname Management** (`sethostname`)
Allows the container to have its own hostname, providing isolation at the UTS (Unix Timesharing System) level. This is particularly useful for multi-container environments where hostname uniqueness is required.

- **Temporary Filesystems** (`tmpfs`)
`tmpfs` is mounted for `/dev` to create an isolated and writable environment for device files. This ensures the container operates independently of the host's `/dev`.

- **Proc and Sys Filesystems** (`proc`, `sysfs`)
These are mounted inside the container to provide system-level information and kernel interfaces specific to the container's process namespace. `/proc` is essential for process-related metadata, while `/sys` is used for interacting with kernel features.

[src-features-runtime-core.rs]: /src/features/runtime/core.rs