// Copyright (c) 2016 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;
use std::ffi::OsString;

use common::ui::UI;
use hcore::crypto::CACHE_KEY_PATH_ENV_VAR;
use hcore::env as henv;
use hcore::fs::CACHE_KEY_PATH;
use hcore::os::users;

use config;
use error::Result;

pub fn start(ui: &mut UI, args: Vec<OsString>) -> Result<()> {
    // If the `$HAB_ORIGIN` environment variable is not present, then see if a default is set in
    // the CLI config. If so, set it as the `$HAB_ORIGIN` environment variable for the `hab-studio`
    // or `docker` execv call.
    if henv::var("HAB_ORIGIN").is_err() {
        let config = try!(config::load_with_sudo_user());
        if let Some(default_origin) = config.origin {
            debug!("Setting default origin {} via CLI config", &default_origin);
            env::set_var("HAB_ORIGIN", default_origin);
        }
    }

    // If the `$HAB_CACHE_KEY_PATH` environment variable is not present, check if we are running
    // under a `sudo` invocation. If so, determine the non-root user that issued the command in
    // order to set their key cache location in the environment variable. This is done so that the
    // `hab-studio` command will find the correct key cache or so that the correct directory will
    // be volume mounted when used with Docker.
    if henv::var(CACHE_KEY_PATH_ENV_VAR).is_err() {
        if let Some(sudo_user) = henv::sudo_user() {
            if let Some(home) = users::get_home_for_user(&sudo_user) {
                let cache_key_path = home.join(format!(".{}", CACHE_KEY_PATH));
                debug!("Setting cache_key_path for SUDO_USER={} to: {}",
                       &sudo_user,
                       cache_key_path.display());
                env::set_var(CACHE_KEY_PATH_ENV_VAR, cache_key_path);
                // Prevent any inner `hab` invocations from triggering similar logic: we will be
                // operating in the context `hab-studio` which is running with rootlike privileges.
                env::remove_var("SUDO_USER");
            }
        }
    }

    inner::start(ui, args)
}

#[cfg(target_os = "linux")]
mod inner {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::str::FromStr;

    use common::ui::UI;
    use hcore::crypto::{init, default_cache_key_path};
    use hcore::env as henv;
    use hcore::fs::find_command;
    use hcore::package::PackageIdent;

    use error::{Error, Result};
    use exec;

    const STUDIO_CMD: &'static str = "hab-studio";
    const STUDIO_CMD_ENVVAR: &'static str = "HAB_STUDIO_BINARY";
    const STUDIO_PACKAGE_IDENT: &'static str = "core/hab-studio";

    pub fn start(ui: &mut UI, args: Vec<OsString>) -> Result<()> {
        let command = match henv::var(STUDIO_CMD_ENVVAR) {
            Ok(command) => PathBuf::from(command),
            Err(_) => {
                init();
                let ident = try!(PackageIdent::from_str(STUDIO_PACKAGE_IDENT));
                try!(exec::command_from_pkg(ui,
                                            STUDIO_CMD,
                                            &ident,
                                            &default_cache_key_path(None),
                                            0))
            }
        };

        if let Some(cmd) = find_command(command.to_string_lossy().as_ref()) {
            try!(exec::exec_command(cmd, args));
        } else {
            return Err(Error::ExecCommandNotFound(command.to_string_lossy().into_owned()));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
mod inner {
    use std::env;
    use std::ffi::OsString;
    use std::process::{Command, Stdio, exit};

    use common::ui::UI;
    use hcore::crypto::default_cache_key_path;
    use hcore::env as henv;
    use hcore::fs::{CACHE_KEY_PATH, find_command};

    use error::{Error, Result};
    use VERSION;

    const DOCKER_CMD: &'static str = "docker";
    const DOCKER_CMD_ENVVAR: &'static str = "HAB_DOCKER_BINARY";
    const DOCKER_IMAGE: &'static str = "habitat-docker-registry.bintray.io/studio";
    const DOCKER_IMAGE_ENVVAR: &'static str = "HAB_DOCKER_STUDIO_IMAGE";

    pub fn start(_ui: &mut UI, args: Vec<OsString>) -> Result<()> {
        let docker = henv::var(DOCKER_CMD_ENVVAR).unwrap_or(DOCKER_CMD.to_string());

        let cmd = match find_command(&docker) {
            Some(cmd) => cmd,
            None => return Err(Error::ExecCommandNotFound(docker.to_string())),
        };

        let child = Command::new(&cmd)
            .arg("pull")
            .arg(&image_identifier())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("docker failed to start");

        let output = child.wait_with_output()
            .expect("failed to wait on child");

        if output.status.success() {
            debug!("Docker image is reachable. Proceeding with launching docker.");
        } else {
            debug!("Docker image is unreachable. Exit code = {:?}",
                   output.status);

            let err_output = String::from_utf8(output.stderr).unwrap();

            if err_output.contains("image") && err_output.contains("not found") {
                return Err(Error::DockerImageNotFound(image_identifier().to_string()));
            } else if err_output.contains("Cannot connect to the Docker daemon") {
                return Err(Error::DockerDaemonDown);
            } else {
                return Err(Error::DockerNetworkDown(image_identifier().to_string()));
            }
        }

        let mut command = Command::new(&cmd);
        command.arg("run")
            .arg("--rm")
            .arg("--tty")
            .arg("--interactive")
            .arg("--privileged");

        let env_vars = vec!["HAB_DEPOT_URL", "HAB_ORIGIN", "http_proxy", "https_proxy"];
        for var in env_vars {
            if let Ok(val) = henv::var(var) {
                debug!("Propagating environment variable into container: {}={}",
                       var,
                       val);
                command.arg("--env");
                command.arg(format!("{}={}", var, val));
            }
        }

        command.arg("--volume")
            .arg("/var/run/docker.sock:/var/run/docker.sock")
            .arg("--volume")
            .arg(format!("{}:/{}",
                         default_cache_key_path(None).to_string_lossy(),
                         CACHE_KEY_PATH))
            .arg("--volume")
            .arg(format!("{}:/src", env::current_dir().unwrap().to_string_lossy()))
            .arg(image_identifier());

        for arg in &args {
            command.arg(arg);
        }

        for var in vec!["http_proxy", "https_proxy"] {
            if let Ok(_) = henv::var(var) {
                debug!("Unsetting proxy environment variable '{}' before calling `{}'",
                       var,
                       docker);
                env::remove_var(var);
            }
        }

        let status = command.status().expect(&(format!("{:?} failed to start.", &cmd)));
        // Replace with specific errors based on exit code?
        // https://docs.docker.com/engine/reference/run/#/exit-status
        // this currently just passes the exit code from Docker directly.
        exit(status.code().unwrap())
    }

    /// Returns the Docker Studio image with tag for the desired version which corresponds to the
    /// same version (minus release) as this program.
    fn image_identifier() -> String {
        let version: Vec<&str> = VERSION.split("/").collect();
        henv::var(DOCKER_IMAGE_ENVVAR).unwrap_or(format!("{}:{}", DOCKER_IMAGE, version[0]))
    }

    #[cfg(test)]
    mod tests {
        use super::{image_identifier, DOCKER_IMAGE};
        use VERSION;

        #[test]
        fn retrieve_image_identifier() {
            assert_eq!(image_identifier(), format!("{}:{}", DOCKER_IMAGE, VERSION));
        }
    }
}
