use anyhow::{Context, Result};
use itertools::Itertools;
use util::ResultExt;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_12_0,
            D3D_FEATURE_LEVEL_12_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Direct3D12::{D3D12CreateDevice, ID3D12Device},
        Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS,
            IDXGIAdapter1, IDXGIFactory6,
        },
    },
};
use windows::core::Interface;

pub(crate) const DIRECTX_BACKEND_ENV: &str = "GPUI_WINDOWS_DIRECTX_BACKEND";

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectXBackend {
    Direct3d11,
    Direct3d12,
}

#[derive(Clone, Debug)]
pub(crate) struct DirectXBackendProbe {
    pub(crate) d3d11_feature_level: D3D_FEATURE_LEVEL,
    pub(crate) d3d12_feature_level: Option<D3D_FEATURE_LEVEL>,
    pub(crate) d3d12_probe_error: Option<String>,
}

impl DirectXBackendProbe {
    fn detect(adapter: &IDXGIAdapter1, d3d11_feature_level: D3D_FEATURE_LEVEL) -> Self {
        match probe_d3d12_feature_level(adapter) {
            Ok(feature_level) => Self {
                d3d11_feature_level,
                d3d12_feature_level: Some(feature_level),
                d3d12_probe_error: None,
            },
            Err(error) => Self {
                d3d11_feature_level,
                d3d12_feature_level: None,
                d3d12_probe_error: Some(format!("{error:#}")),
            },
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectXBackendRequest {
    Automatic,
    Direct3d11,
    Direct3d12,
}

impl DirectXBackendRequest {
    fn from_env() -> (Self, Option<String>) {
        let Ok(value) = std::env::var(DIRECTX_BACKEND_ENV) else {
            return (Self::Automatic, None);
        };

        let normalized = value.trim().to_ascii_lowercase();
        let request = match normalized.as_str() {
            "11" | "d3d11" | "direct3d11" | "directx11" | "dx11" => Self::Direct3d11,
            "12" | "d3d12" | "direct3d12" | "directx12" | "dx12" => Self::Direct3d12,
            _ => {
                log::warn!(
                    "Ignoring unsupported value {:?} for {}. Supported values are: 11, 12. Probing the adapter and preferring Direct3D 12 when available, otherwise Direct3D 11.",
                    value,
                    DIRECTX_BACKEND_ENV
                );
                Self::Automatic
            }
        };
        (request, Some(value))
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Automatic => "Auto",
            Self::Direct3d11 => "Direct3D 11",
            Self::Direct3d12 => "Direct3D 12",
        }
    }
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    /// 当前实际启用的后端。
    /// 无显式配置时会在启动探测阶段优先选 Direct3D 12，否则退到 Direct3D 11。
    /// 一旦选定，不会在启动完成后或恢复阶段跨后端切换。
    active_backend: DirectXBackend,
    pub(crate) backend_probe: DirectXBackendProbe,
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    d3d12_device: Option<ID3D12Device>,
    backend_request: DirectXBackendRequest,
    backend_request_raw: Option<String>,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let (adapter, device, device_context, feature_level) =
            get_adapter(&dxgi_factory, debug_layer_available).context("Getting DXGI adapter")?;
        let backend_probe = DirectXBackendProbe::detect(&adapter, feature_level);
        let (backend_request, backend_request_raw) = DirectXBackendRequest::from_env();
        let d3d12_device = backend_probe
            .d3d12_feature_level
            .map(|feature_level| {
                create_d3d12_device(&adapter, feature_level)
                    .context("Creating cached Direct3D 12 device")
            })
            .transpose()?;

        let active_backend = match backend_request {
            DirectXBackendRequest::Automatic => {
                if d3d12_device.is_some() {
                    DirectXBackend::Direct3d12
                } else {
                    DirectXBackend::Direct3d11
                }
            }
            DirectXBackendRequest::Direct3d11 => DirectXBackend::Direct3d11,
            DirectXBackendRequest::Direct3d12 => {
                if d3d12_device.is_some() {
                    DirectXBackend::Direct3d12
                } else {
                    anyhow::bail!(
                        "{} requested Direct3D 12, but the current adapter does not expose a usable D3D12 device.",
                        DIRECTX_BACKEND_ENV
                    );
                }
            }
        };

        let d3d11_feature_level = display_feature_level(backend_probe.d3d11_feature_level);
        match (active_backend, backend_probe.d3d12_feature_level) {
            (DirectXBackend::Direct3d12, Some(d3d12_feature_level)) => {
                log::info!(
                    "Direct3D backend probe: adapter supports Direct3D 12 feature level {}. Direct3D 11 compatibility feature level is {}.",
                    display_feature_level(d3d12_feature_level),
                    d3d11_feature_level
                );
            }
            (DirectXBackend::Direct3d11, None)
                if backend_request == DirectXBackendRequest::Automatic =>
            {
                log::info!(
                    "Direct3D backend probe: adapter does not expose a usable Direct3D 12 device, selecting Direct3D 11 with feature level {}.",
                    d3d11_feature_level
                );
                if let Some(error) = &backend_probe.d3d12_probe_error {
                    log::info!("Direct3D 12 probe failed on current adapter: {error}");
                }
            }
            (DirectXBackend::Direct3d11, Some(d3d12_feature_level)) => {
                log::info!(
                    "Direct3D backend probe: Direct3D 11 is selected with feature level {}. Current adapter also supports Direct3D 12 feature level {}.",
                    d3d11_feature_level,
                    display_feature_level(d3d12_feature_level)
                );
            }
            (DirectXBackend::Direct3d11, None) => {
                log::info!(
                    "Direct3D backend probe: Direct3D 11 is selected with feature level {}.",
                    d3d11_feature_level
                );
                if let Some(error) = &backend_probe.d3d12_probe_error {
                    log::info!("Direct3D 12 probe failed on current adapter: {error}");
                }
            }
            (DirectXBackend::Direct3d12, None) => unreachable!(),
        }

        Ok(Self {
            active_backend,
            backend_probe,
            adapter,
            dxgi_factory,
            device,
            device_context,
            d3d12_device,
            backend_request,
            backend_request_raw,
        })
    }

    pub(crate) fn active_backend(&self) -> DirectXBackend {
        self.active_backend
    }

    pub(crate) fn d3d12_device(&self) -> Option<&ID3D12Device> {
        self.d3d12_device.as_ref()
    }

    pub(crate) fn backend_request(&self) -> DirectXBackendRequest {
        self.backend_request
    }

    pub(crate) fn backend_request_raw(&self) -> Option<&str> {
        self.backend_request_raw.as_deref()
    }

    pub(crate) fn with_active_backend(&self, active_backend: DirectXBackend) -> Self {
        let mut devices = self.clone();
        devices.active_backend = active_backend;
        devices
    }

    pub(crate) fn adapter_name(&self) -> Result<String> {
        let desc = unsafe { self.adapter.GetDesc1() }?;
        Ok(String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string())
    }

    pub(crate) fn d3d11_feature_level(&self) -> D3D_FEATURE_LEVEL {
        self.backend_probe.d3d11_feature_level
    }

    pub(crate) fn d3d12_feature_level(&self) -> Option<D3D_FEATURE_LEVEL> {
        self.backend_probe.d3d12_feature_level
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

#[inline]
fn get_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<(
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
)> {
    for adapter_index in 0.. {
        let adapter: IDXGIAdapter1 = unsafe { dxgi_factory.EnumAdapters(adapter_index)?.cast()? };
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11 and create
        // the device if it does.
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            return Ok((adapter, device, context.unwrap(), feature_level));
        }
    }

    unreachable!()
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )?;
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}

#[inline]
fn probe_d3d12_feature_level(adapter: &IDXGIAdapter1) -> Result<D3D_FEATURE_LEVEL> {
    for &feature_level in &[D3D_FEATURE_LEVEL_12_1, D3D_FEATURE_LEVEL_12_0] {
        if create_d3d12_device(adapter, feature_level).is_ok() {
            return Ok(feature_level);
        }
    }

    Err(anyhow::anyhow!(
        "Direct3D 12 feature level 12_0 or newer is unavailable on the selected adapter"
    ))
}

#[inline]
pub(crate) fn create_d3d12_device(
    adapter: &IDXGIAdapter1,
    feature_level: D3D_FEATURE_LEVEL,
) -> Result<ID3D12Device> {
    let mut device = None;
    unsafe {
        D3D12CreateDevice(adapter, feature_level, &mut device)
            .context("Creating Direct3D 12 device")?;
    }
    device.context("Direct3D 12 device is missing after creation")
}

#[inline]
pub(crate) fn display_feature_level(feature_level: D3D_FEATURE_LEVEL) -> &'static str {
    match feature_level {
        D3D_FEATURE_LEVEL_12_1 => "12.1",
        D3D_FEATURE_LEVEL_12_0 => "12.0",
        D3D_FEATURE_LEVEL_11_1 => "11.1",
        D3D_FEATURE_LEVEL_11_0 => "11.0",
        D3D_FEATURE_LEVEL_10_1 => "10.1",
        _ => "unknown",
    }
}

impl DirectXBackend {
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Direct3d11 => "Direct3D 11",
            Self::Direct3d12 => "Direct3D 12",
        }
    }
}
