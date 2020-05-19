// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//#![deny(warnings)]

use std::fs::File;

use vmm_config::boot_source::{
    BootConfig, BootSourceConfig, BootSourceConfigError, DEFAULT_KERNEL_CMDLINE,
};
use vmm_config::fs::*;
use vmm_config::logger::LoggerConfigError;
use vmm_config::machine_config::{VmConfig, VmConfigError};
use vmm_config::vsock::*;
use vstate::VcpuConfig;

type Result<E> = std::result::Result<(), E>;

/// Errors encountered when configuring microVM resources.
#[derive(Debug)]
pub enum Error {
    /// JSON is invalid.
    InvalidJson,
    /// Boot source configuration error.
    BootSource(BootSourceConfigError),
    /// Fs device configuration error.
    FsDevice(FsConfigError),
    /// Logger configuration error.
    Logger(LoggerConfigError),
    /// microVM vCpus or memory configuration error.
    VmConfig(VmConfigError),
    /// Vsock device configuration error.
    VsockDevice(VsockConfigError),
}

/// A data structure that encapsulates the device configurations
/// held in the Vmm.
#[derive(Default)]
pub struct VmResources {
    /// The vCpu and memory configuration for this microVM.
    vm_config: VmConfig,
    /// The boot configuration for this microVM.
    boot_config: Option<BootConfig>,
    /// The fs device.
    pub fs: FsBuilder,
    /// The vsock device.
    pub vsock: VsockBuilder,
}

impl VmResources {
    /// Returns a VcpuConfig based on the vm config.
    pub fn vcpu_config(&self) -> VcpuConfig {
        // The unwraps are ok to use because the values are initialized using defaults if not
        // supplied by the user.
        VcpuConfig {
            vcpu_count: self.vm_config().vcpu_count.unwrap(),
            ht_enabled: self.vm_config().ht_enabled.unwrap(),
            cpu_template: self.vm_config().cpu_template,
        }
    }

    /// Returns the VmConfig.
    pub fn vm_config(&self) -> &VmConfig {
        &self.vm_config
    }

    /// Set the machine configuration of the microVM.
    pub fn set_vm_config(&mut self, machine_config: &VmConfig) -> Result<VmConfigError> {
        if machine_config.vcpu_count == Some(0) {
            return Err(VmConfigError::InvalidVcpuCount);
        }

        if machine_config.mem_size_mib == Some(0) {
            return Err(VmConfigError::InvalidMemorySize);
        }

        let ht_enabled = machine_config
            .ht_enabled
            .unwrap_or_else(|| self.vm_config.ht_enabled.unwrap());

        let vcpu_count_value = machine_config
            .vcpu_count
            .unwrap_or_else(|| self.vm_config.vcpu_count.unwrap());

        // If hyperthreading is enabled or is to be enabled in this call
        // only allow vcpu count to be 1 or even.
        if ht_enabled && vcpu_count_value > 1 && vcpu_count_value % 2 == 1 {
            return Err(VmConfigError::InvalidVcpuCount);
        }

        // Update all the fields that have a new value.
        self.vm_config.vcpu_count = Some(vcpu_count_value);
        self.vm_config.ht_enabled = Some(ht_enabled);

        if machine_config.mem_size_mib.is_some() {
            self.vm_config.mem_size_mib = machine_config.mem_size_mib;
        }

        if machine_config.cpu_template.is_some() {
            self.vm_config.cpu_template = machine_config.cpu_template;
        }

        Ok(())
    }

    /// Gets a reference to the boot source configuration.
    pub fn boot_source(&self) -> Option<&BootConfig> {
        self.boot_config.as_ref()
    }

    /// Set the guest boot source configuration.
    pub fn set_boot_source(
        &mut self,
        boot_source_cfg: BootSourceConfig,
    ) -> Result<BootSourceConfigError> {
        use self::BootSourceConfigError::{
            InvalidInitrdPath, InvalidKernelCommandLine, InvalidKernelPath,
        };

        // Validate boot source config.
        let kernel_file =
            File::open(&boot_source_cfg.kernel_image_path).map_err(InvalidKernelPath)?;
        let initrd_file: Option<File> = match &boot_source_cfg.initrd_path {
            Some(path) => Some(File::open(path).map_err(InvalidInitrdPath)?),
            None => None,
        };
        let mut cmdline = kernel::cmdline::Cmdline::new(arch::CMDLINE_MAX_SIZE);
        let boot_args = match boot_source_cfg.boot_args.as_ref() {
            None => DEFAULT_KERNEL_CMDLINE,
            Some(str) => str.as_str(),
        };
        cmdline
            .insert_str(boot_args)
            .map_err(|e| InvalidKernelCommandLine(e.to_string()))?;

        self.boot_config = Some(BootConfig {
            cmdline,
            kernel_file,
            initrd_file,
        });
        Ok(())
    }

    pub fn set_fs_device(&mut self, config: FsDeviceConfig) -> Result<FsConfigError> {
        self.fs.insert(config)
    }

    /// Sets a vsock device to be attached when the VM starts.
    pub fn set_vsock_device(&mut self, config: VsockDeviceConfig) -> Result<VsockConfigError> {
        self.vsock.insert(config)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::linux::fs::MetadataExt;

    use super::*;
    use logger::{LevelFilter, LOGGER};
    use resources::VmResources;
    use utils::tempfile::TempFile;
    use vmm_config::boot_source::{BootConfig, BootSourceConfig, DEFAULT_KERNEL_CMDLINE};
    use vmm_config::machine_config::{CpuFeaturesTemplate, VmConfig, VmConfigError};
    use vmm_config::net::{NetBuilder, NetworkInterfaceConfig};
    use vmm_config::vsock::tests::{default_config, TempSockFile};
    use vmm_config::RateLimiterConfig;
    use vstate::VcpuConfig;

    fn default_net_cfg() -> NetworkInterfaceConfig {
        NetworkInterfaceConfig {
            iface_id: "net_if1".to_string(),
            // TempFile::new_with_prefix("") generates a random file name used as random net_if name.
            host_dev_name: TempFile::new_with_prefix("")
                .unwrap()
                .as_path()
                .to_str()
                .unwrap()
                .to_string(),
            guest_mac: Some(MacAddr::parse_str("01:23:45:67:89:0a").unwrap()),
            allow_mmds_requests: false,
        }
    }

    fn default_boot_cfg() -> BootConfig {
        let mut kernel_cmdline = kernel::cmdline::Cmdline::new(4096);
        kernel_cmdline.insert_str(DEFAULT_KERNEL_CMDLINE).unwrap();
        let tmp_file = TempFile::new().unwrap();
        BootConfig {
            cmdline: kernel_cmdline,
            kernel_file: Some(File::open(tmp_file.as_path()).unwrap()),
            initrd_file: Some(File::open(tmp_file.as_path()).unwrap()),
        }
    }

    fn default_vm_resources() -> VmResources {
        VmResources {
            vm_config: VmConfig::default(),
            boot_config: Some(default_boot_cfg()),
            block: default_blocks(),
            vsock: Default::default(),
            net_builder: default_net_builder(),
            mmds_config: None,
        }
    }

    impl PartialEq for BootConfig {
        fn eq(&self, other: &Self) -> bool {
            self.cmdline.as_str().eq(other.cmdline.as_str())
                && self.kernel_file.metadata().unwrap().st_ino()
                    == other.kernel_file.metadata().unwrap().st_ino()
                && self
                    .initrd_file
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .st_ino()
                    == other
                        .initrd_file
                        .as_ref()
                        .unwrap()
                        .metadata()
                        .unwrap()
                        .st_ino()
        }
    }

    #[test]
    fn test_vcpu_config() {
        let vm_resources = default_vm_resources();
        let expected_vcpu_config = VcpuConfig {
            vcpu_count: vm_resources.vm_config().vcpu_count.unwrap(),
            ht_enabled: vm_resources.vm_config().ht_enabled.unwrap(),
            cpu_template: vm_resources.vm_config().cpu_template,
        };

        let vcpu_config = vm_resources.vcpu_config();
        assert_eq!(vcpu_config, expected_vcpu_config);
    }

    #[test]
    fn test_vm_config() {
        let vm_resources = default_vm_resources();
        let expected_vm_cfg = VmConfig::default();

        assert_eq!(vm_resources.vm_config(), &expected_vm_cfg);
    }

    #[test]
    fn test_set_vm_config() {
        let mut vm_resources = default_vm_resources();
        let mut aux_vm_config = VmConfig {
            vcpu_count: Some(32),
            mem_size_mib: Some(512),
            ht_enabled: Some(true),
            cpu_template: Some(CpuFeaturesTemplate::T2),
        };

        assert_ne!(vm_resources.vm_config, aux_vm_config);
        vm_resources.set_vm_config(&aux_vm_config).unwrap();
        assert_eq!(vm_resources.vm_config, aux_vm_config);

        // Invalid vcpu count.
        aux_vm_config.vcpu_count = Some(0);
        assert_eq!(
            vm_resources.set_vm_config(&aux_vm_config),
            Err(VmConfigError::InvalidVcpuCount)
        );
        aux_vm_config.vcpu_count = Some(33);
        assert_eq!(
            vm_resources.set_vm_config(&aux_vm_config),
            Err(VmConfigError::InvalidVcpuCount)
        );
        aux_vm_config.vcpu_count = Some(32);

        // Invalid mem_size_mib.
        aux_vm_config.mem_size_mib = Some(0);
        assert_eq!(
            vm_resources.set_vm_config(&aux_vm_config),
            Err(VmConfigError::InvalidMemorySize)
        );
    }

    #[test]
    fn test_boot_config() {
        let vm_resources = default_vm_resources();
        let expected_boot_cfg = vm_resources.boot_config.as_ref().unwrap();
        let actual_boot_cfg = vm_resources.boot_source().unwrap();

        assert_eq!(actual_boot_cfg, expected_boot_cfg);
    }

    #[test]
    fn test_set_boot_source() {
        let tmp_file = TempFile::new().unwrap();
        let cmdline = "reboot=k panic=1 pci=off nomodules 8250.nr_uarts=0";
        let expected_boot_cfg = BootSourceConfig {
            kernel_image_path: String::from(tmp_file.as_path().to_str().unwrap()),
            initrd_path: Some(String::from(tmp_file.as_path().to_str().unwrap())),
            boot_args: Some(cmdline.to_string()),
        };

        let mut vm_resources = default_vm_resources();
        let boot_cfg = vm_resources.boot_source().unwrap();
        let tmp_ino = tmp_file.as_file().metadata().unwrap().st_ino();

        assert_ne!(boot_cfg.cmdline.as_str(), cmdline);
        assert_ne!(boot_cfg.kernel_file.metadata().unwrap().st_ino(), tmp_ino);
        assert_ne!(
            boot_cfg
                .initrd_file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .st_ino(),
            tmp_ino
        );

        vm_resources.set_boot_source(expected_boot_cfg).unwrap();
        let boot_cfg = vm_resources.boot_source().unwrap();
        assert_eq!(boot_cfg.cmdline.as_str(), cmdline);
        assert_eq!(boot_cfg.kernel_file.metadata().unwrap().st_ino(), tmp_ino);
        assert_eq!(
            boot_cfg
                .initrd_file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .st_ino(),
            tmp_ino
        );
    }

    #[test]
    fn test_set_block_device() {
        let mut vm_resources = default_vm_resources();
        let (mut new_block_device_cfg, _file) = default_block_cfg();
        let tmp_file = TempFile::new().unwrap();
        new_block_device_cfg.drive_id = "block2".to_string();
        new_block_device_cfg.path_on_host = tmp_file.as_path().to_str().unwrap().to_string();
        assert_eq!(vm_resources.block.list.len(), 1);
        vm_resources.set_block_device(new_block_device_cfg).unwrap();
        assert_eq!(vm_resources.block.list.len(), 2);
    }

    #[test]
    fn test_set_vsock_device() {
        let mut vm_resources = default_vm_resources();
        let tmp_sock_file = TempSockFile::new(TempFile::new().unwrap());
        let new_vsock_cfg = default_config(&tmp_sock_file);
        assert!(vm_resources.vsock.get().is_none());
        vm_resources
            .set_vsock_device(new_vsock_cfg.clone())
            .unwrap();
        let actual_vsock_cfg = vm_resources.vsock.get().unwrap();
        assert_eq!(
            actual_vsock_cfg.lock().unwrap().id(),
            &new_vsock_cfg.vsock_id
        );
    }

    #[test]
    fn test_set_net_device() {
        let mut vm_resources = default_vm_resources();

        // Clone the existing net config in order to obtain a new one.
        let mut new_net_device_cfg = default_net_cfg();
        new_net_device_cfg.iface_id = "new_net_if".to_string();
        new_net_device_cfg.guest_mac = Some(MacAddr::parse_str("01:23:45:67:89:0c").unwrap());
        new_net_device_cfg.host_dev_name = "dummy_path2".to_string();
        assert_eq!(vm_resources.net_builder.len(), 1);

        vm_resources.build_net_device(new_net_device_cfg).unwrap();
        assert_eq!(vm_resources.net_builder.len(), 2);
    }
}