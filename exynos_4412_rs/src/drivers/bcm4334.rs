//! BCM4334 WiFi chip: its power on this board, loading its firmware, and talking to it over SDIO.
//!
//! This runs the firmware, sends it commands, scans for networks, joins a WPA2-Personal network with
//! [`crate::wpa`], and sends and receives Ethernet frames. IP is left to [`crate::net`].
//!
//! # Sources
//!
//! Broadcom publishes no description of this interface. What is known of it here comes from
//! Linux 5.15.198, `drivers/net/wireless/broadcom/brcm80211/`, which is under the ISC license:
//! see `THIRD_PARTY_NOTICES.md`. Files are cited relative to that directory.
//! `exynos4412-mehmet.dts` gives the board's wiring.
//!
//! The board needs a patch to that driver, `brcmfmac.patch`, which turns off the separate
//! interrupt line and scatter-gather lists, and asks for 4-byte head and 512-byte list alignment.
//! Here everything is polled and nothing uses lists.
//!
//! # Where this differs from Linux
//!
//! - **Clock:** It stays forced on once the firmware runs. The chip is never put to sleep, so the
//!   save-and-restore setup of `brcmfmac/sdio.c:3433-3483` is not needed.
//! - **Glomming:** The firmware is told not to merge frames, which saves handling such frames.
//! - **Drive strength:** Not set. `brcmf_sdio_drivestrengthinit` in `brcmfmac/sdio.c:3773-3840`
//!   writes the value back to the address register, not the data register, so on Linux it does
//!   not reach the setting it is computed for either.
//!
//! # Concurrency
//!
//! One core at a time, like [`Sdio`].

use super::gpio::{self, Drive, Function, Pin, Pull};
use super::max77686::{self, Clock32k, Max77686};
use super::sdhci::{self, Sdhci};
use super::sdio::{self, Sdio, delay_ms};
use super::timer::Mct;
use crate::wpa::{self, Supplicant};
use crate::{nvram, soc};

/// `wlan_lpo_regulator`, enable of the chip's 3 V supply: `exynos4412-mehmet.dts:203-215`.
const LDO_ENABLE: Pin = gpio::GPX1.pin(4);
/// `wlan_pwrseq` and `wlan_en`, the chip's enable: `exynos4412-mehmet.dts:175-184,630-635`.
const WLAN_ENABLE: Pin = gpio::GPM0.pin(4);
/// SD clock: `exynos4412-pinctrl.dtsi:776-781` with `exynos4412-mehmet.dts:1553-1555`.
const SD_CLOCK: Pin = gpio::GPK3.pin(0);
/// SD command: `exynos4412-pinctrl.dtsi:783-788` with `exynos4412-mehmet.dts:1557-1560`.
const SD_COMMAND: Pin = gpio::GPK3.pin(1);
/// SD data 0 to 3: `exynos4412-pinctrl.dtsi:804-809` with `exynos4412-mehmet.dts:1548-1551`.
const SD_DATA: [Pin; 4] = [gpio::GPK3.pin(3), gpio::GPK3.pin(4), gpio::GPK3.pin(5), gpio::GPK3.pin(6)];
/// `EXYNOS_PIN_FUNC_2` at the lines above.
const SD_FUNCTION: Function = Function::Alt(2);

/// `ldo22_reg`, `vdd_bt_wifi`: `exynos4412-mehmet.dts:1114-1120`, and the same voltage in the
/// vendor kernel, `mach-mehmet.c:1829-1833`.
const LDO_WIFI: (u8, u16) = (22, 3300);
/// `exynos4412-mehmet.dts:208` `startup-delay-us`.
const LDO_ENABLE_SETTLE_MS: u64 = 200;
/// How long reset stays asserted. Not measured, and well over what a reset needs.
const RESET_MS: u64 = 50;
/// `exynos4412-mehmet.dts:183` `post-power-on-delay-ms`.
const AFTER_RESET_MS: u64 = 200;
/// Rail settling. Not measured: see `pmic_test`.
const RAIL_SETTLE_MS: u64 = 10;

/// Card identification rate.
const IDENTIFY_HZ: u32 = 400_000;
/// `exynos4412-mehmet.dts:1514` `max-frequency`, the rate at which Linux worked on this board.
const DATA_HZ: u32 = 16_000_000;

/// The function with the backplane window and the chip's clock control.
const FUNCTION1: u8 = 1;
/// The function that carries frames.
const FUNCTION2: u8 = 2;
/// `brcmfmac/bcmsdh.c:44`.
const FUNCTION1_BLOCK_SIZE: u16 = 64;
/// `brcmfmac/bcmsdh.c:45`.
const FUNCTION2_BLOCK_SIZE: u16 = 512;
/// `brcmfmac/bcmsdh.c:50` `SDIO_WAIT_F2RDY`.
const FUNCTION2_READY_MS: u64 = 3000;
/// How long a function 1 enable takes, as a timeout. Not measured.
const FUNCTION1_READY_MS: u64 = 1000;

/// `brcmfmac/sdio.h:43`: Broadcom's own register in function 0.
const CCCR_BRCM_CARD_CONTROL: u32 = 0xF1;
/// `brcmfmac/sdio.h:44`.
const CARD_CONTROL_WLAN_RESET: u8 = 1 << 1;

/// `brcmfmac/sdio.h:70`.
const REG_WATERMARK: u32 = 0x10008;
/// `brcmfmac/sdio.c:43` `DEFAULT_F2_WATERMARK`.
const DEFAULT_WATERMARK: u8 = 0x8;
/// `brcmfmac/sdio.h:75,77,79`: the three bytes of the backplane window, low first.
const REG_WINDOW_LOW: u32 = 0x1000A;
/// `brcmfmac/sdio.h:83`.
const REG_CHIP_CLOCK: u32 = 0x1000E;
/// `brcmfmac/sdio.h:85`.
const REG_PULL_UPS: u32 = 0x1000F;
/// `brcmfmac/sdio.h:109,112`.
const REG_SLEEP: u32 = 0x1001F;
const SLEEP_KEEP_ON: u8 = 1;
/// `brcmfmac/sdio.c:3500`: the sleep register appears in this revision of the SDIO core.
const SDIO_CORE_REV_KEEP_ON: u8 = 12;

/// `brcmfmac/sdio.c:185,189`: ask for the slow clock, and keep the hardware from asking for more.
const CLOCK_REQUEST_ALP: u8 = 0x08 | 0x20;
/// `brcmfmac/sdio.c:185`: the same, as the driver asks while it loads firmware.
const CLOCK_ALP_ONLY: u8 = 0x08;
/// `brcmfmac/sdio.c:187`.
const CLOCK_REQUEST_HT: u8 = 0x10;
/// `brcmfmac/sdio.c:181`.
const CLOCK_FORCE_HT: u8 = 0x02;
/// `brcmfmac/sdio.c:191`.
const CLOCK_ALP_AVAILABLE: u8 = 0x40;
/// `brcmfmac/sdio.c:193`.
const CLOCK_HT_AVAILABLE: u8 = 0x80;
/// `brcmfmac/sdio.c:179,189`.
const CLOCK_FORCE_ALP: u8 = 0x01 | 0x20;
/// `brcmfmac/sdio.c:195`.
const CLOCK_AVAILABLE_BITS: u8 = 0xC0;
/// `brcmfmac/sdio.c:314` `PMU_MAX_TRANSITION_DLY`.
const CLOCK_TIMEOUT_MS: u64 = 1000;
/// `brcmfmac/sdio.c:3879`: after forcing the slow clock.
const CLOCK_FORCE_SETTLE_US: u64 = 65;

/// `brcmfmac/sdio.h:122,123,125,128`.
const WINDOW_OFFSET_MASK: u32 = 0x7FFF;
const WINDOW_SIZE: u32 = 0x8000;
const WINDOW_ACCESS_32BIT: u32 = 0x8000;
const WINDOW_MASK: u32 = 0xFFFF_8000;

/// `include/soc.h:9` `SI_ENUM_BASE_DEFAULT`: the chip's ID register is the first of this core,
/// by `brcmfmac/chip.c:963`.
const ENUM_BASE: u32 = 0x1800_0000;
/// `include/brcm_hw_ids.h:27`.
pub const CHIP_ID_4334: u16 = 0x4334;
/// `include/chipcommon.h:222-224`.
const CID_ID_MASK: u32 = 0xFFFF;
const CID_REVISION_SHIFT: u32 = 16;
const CID_REVISION_MASK: u32 = 0xF;
/// `include/chipcommon.h:229-230`.
const CID_TYPE_SHIFT: u32 = 28;
const CID_TYPE_MASK: u32 = 0xF;
/// `brcmfmac/chip.c:22` `SOCI_AI`.
const BACKPLANE_AI: u8 = 1;

/// Registers of the chip's common core: `struct chipcregs`, `include/chipcommon.h:14-204`.
const CC_CAPABILITIES: u32 = 0x04;
const CC_CAPABILITIES_EXT: u32 = 0xAC;
const CC_EROM_POINTER: u32 = 0xFC;
const CC_PMU_CONTROL: u32 = 0x600;
const CC_PMU_CAPABILITIES: u32 = 0x604;
/// `include/chipcommon.h:252`.
const CC_CAP_PMU: u32 = 0x1000_0000;
/// `include/chipcommon.h:281`.
const PMU_CAP_REV_MASK: u32 = 0xFF;
/// `include/linux/bcma/bcma_driver_chipcommon.h:326-327`: a backplane reset reloads PMU state.
const PMU_CONTROL_RESOURCE_RELOAD: u32 = 0x2 << 13;
/// `include/linux/bcma/bcma_driver_chipcommon.h:225`.
const CC_CAP_EXT_AOB: u32 = 0x40;
/// `brcmfmac/chip.c:1208`: a separate PMU core exists only from this revision of the common core.
const CC_REV_SEPARATE_PMU: u8 = 35;

/// `brcmfmac/chip.c:25-32`: the kinds of entry in the chip's table of cores.
const DESC_TYPE_MASK: u32 = 0xF;
const DESC_VALID: u32 = 0x1;
const DESC_COMPONENT: u32 = 0x1;
const DESC_MASTER_PORT: u32 = 0x3;
const DESC_ADDRESS: u32 = 0x5;
const DESC_SIZE_GT32: u32 = 0x8;
const DESC_END: u32 = 0xF;
/// `brcmfmac/chip.c:36-45`.
const COMP_PART_SHIFT: u32 = 8;
const COMP_PART_MASK: u32 = 0xFFF;
const COMP_REVISION_SHIFT: u32 = 24;
const COMP_SWRAP_SHIFT: u32 = 19;
const COMP_MWRAP_SHIFT: u32 = 14;
const COMP_WRAP_MASK: u32 = 0x1F;
/// `brcmfmac/chip.c:56-71`.
const SLAVE_BASE_MASK: u32 = 0xFFFF_F000;
const SLAVE_TYPE_SHIFT: u32 = 6;
const SLAVE_TYPE_MASK: u32 = 0x3;
const SLAVE_TYPE_SLAVE: u32 = 0;
const SLAVE_TYPE_SWRAP: u32 = 2;
const SLAVE_TYPE_MWRAP: u32 = 3;
const SLAVE_SIZE_SHIFT: u32 = 4;
const SLAVE_SIZE_MASK: u32 = 0x3;
const SLAVE_SIZE_8K: u32 = 1;
const SLAVE_SIZE_DESC: u32 = 3;
/// A table longer than this has no end.
const EROM_MAX_ENTRIES: u32 = 1024;

/// `include/linux/bcma/bcma.h:97`.
const CORE_COMMON: u16 = 0x800;
/// `include/linux/bcma/bcma.h:111`.
const CORE_RAM: u16 = 0x80E;
/// `include/linux/bcma/bcma.h:115`.
const CORE_80211: u16 = 0x812;
/// `include/linux/bcma/bcma.h:136`.
const CORE_PMU: u16 = 0x827;
/// `include/linux/bcma/bcma.h:138`.
const CORE_SDIO: u16 = 0x829;
/// `include/linux/bcma/bcma.h:139`.
const CORE_ARM_CM3: u16 = 0x82A;
/// `include/linux/bcma/bcma.h:156`.
const CORE_GCI: u16 = 0x840;

/// `include/linux/bcma/bcma_regs.h:29-31`: the wrapper's I/O control.
const WRAP_IOCTL: u32 = 0x0408;
const IOCTL_CLOCK: u32 = 0x0001;
const IOCTL_FORCE_GATED: u32 = 0x0002;
/// `include/linux/bcma/bcma_regs.h:41-42`: the wrapper's reset control.
const WRAP_RESET: u32 = 0x0800;
const RESET_ASSERT: u32 = 0x0001;
/// `brcmfmac/chip.c:81,82`.
const D11_PHY_CLOCK: u32 = 0x0004;
const D11_PHY_RESET: u32 = 0x0008;
/// `brcmfmac/chip.c` `brcmf_chip_ai_resetcore`: tries at releasing reset, and the wait after each.
const RESET_RELEASE_TRIES: u32 = 50;
const RESET_RELEASE_WAIT_US: u64 = 60;
/// `brcmfmac/chip.c` `brcmf_chip_ai_coredisable`: wait for reset to take hold.
const RESET_ASSERT_WAIT_US: u64 = 300;

/// Offsets in `struct sbsocramregs`, `brcmfmac/chip.c:165-185`.
const RAM_CORE_INFO: u32 = 0x00;
const RAM_BANK_INDEX: u32 = 0x10;
const RAM_BANK_INFO: u32 = 0x40;
/// `brcmfmac/chip.c:156-163`.
const RAM_BANKS_SHIFT: u32 = 4;
const RAM_BANKS_MASK: u32 = 0xF;
const RAM_BANKS_MASK_EXT: u32 = 0x1F;
const RAM_LAST_SIZE_SHIFT: u32 = 20;
const RAM_LAST_SIZE_MASK: u32 = 0xF;
const RAM_BANK_SIZE_MASK: u32 = 0xF;
const RAM_BANK_SIZE_BASE: u32 = 14;
/// `brcmfmac/chip.c:613`: how old RAM cores give their size.
const RAM_REV_SIMPLE_MAX: u8 = 7;
const RAM_REV_SIMPLE_ALSO: u8 = 12;
/// `brcmfmac/chip.c:623`: bank count gets wider from here.
const RAM_REV_WIDE: u8 = 23;
/// `brcmfmac/chip.c:146,155`.
const RAM_BANK_INFO_SIZE_MASK: u32 = 0x7F;
const RAM_BANK_INFO_SIZE_UNIT: u32 = 8192;
/// A size above this is not a real one: `brcmfmac/chip.c:94` `BRCMF_CHIP_MAX_MEMSIZE`.
const RAM_MAX_SIZE: u32 = 4 * 1024 * 1024;

/// Registers of the SDIO core: `struct sdpcmd_regs`, `brcmfmac/sdio.h:212,213,219,221,222`.
const SDIO_INT_STATUS: u32 = 0x20;
const SDIO_HOST_INT_MASK: u32 = 0x24;
const SDIO_TO_CHIP_MAILBOX: u32 = 0x40;
const SDIO_TO_CHIP_MAILBOX_DATA: u32 = 0x48;
const SDIO_TO_HOST_MAILBOX_DATA: u32 = 0x4C;
/// `brcmfmac/sdio.c:211-213,231,271,272,779`.
const INT_FRAME: u32 = 1 << 6;
const INT_MAILBOX: u32 = 1 << 7;
const INT_ALL_MAILBOXES: u32 = 0xF0;
const INT_CHIP_ACTIVE: u32 = 1 << 29;
const HOST_INT_MASK: u32 = INT_ALL_MAILBOXES | INT_CHIP_ACTIVE;
/// `brcmfmac/sdio.c:257`.
const MAILBOX_ACK: u32 = 1 << 1;
/// `brcmfmac/sdio.c:262,292`.
const MAILBOX_PROTOCOL_VERSION: u32 = 4 << 16;
/// `brcmfmac/sdio.c:279`.
const MAILBOX_FIRMWARE_HALTED: u32 = 0x0010;

/// `brcmfmac/sdio.c:136` `BRCMF_FIRSTREAD`.
const FIRST_READ: usize = 64;
/// `brcmfmac/sdio.c:1327-1330`.
const HEADER_LENGTH: usize = 12;
/// `brcmfmac/sdio.c:1334-1336`.
const CHANNEL_SHIFT: u32 = 8;
const CHANNEL_MASK: u32 = 0xF;
const CHANNEL_CONTROL: u8 = 0;
/// `brcmfmac/sdio.c:1344-1345`.
const DATA_OFFSET_SHIFT: u32 = 24;
/// `brcmfmac/sdio.c:1347-1348`.
const WINDOW_SEQUENCE_SHIFT: u32 = 8;
/// `brcmfmac/sdio.c:4455`: the sequence number of the first frame.
const FIRST_TX_SEQUENCE: u8 = 255;
/// The largest frame this handles. `brcmfmac/sdio.c:306` allows 2048 for data.
const FRAME_SIZE: usize = 4096;

/// `struct brcmf_proto_bcdc_dcmd`, `brcmfmac/bcdc.c:25-31`.
const BCDC_LENGTH: usize = 16;
/// `brcmfmac/bcdc.c:34,35,39`.
const BCDC_ERROR: u32 = 0x01;
const BCDC_SET: u32 = 0x02;
const BCDC_ID_SHIFT: u32 = 16;
/// `brcmfmac/core.h:31`: what one control message may hold.
const CONTROL_MESSAGE_MAX: usize = 1518;
/// `brcmfmac/fwil.h:12`.
pub const IOCTL_GET_VERSION: u32 = 1;
/// `brcmfmac/fwil.h:78`.
const IOCTL_GET_VARIABLE: u32 = 262;
/// `brcmfmac/fwil.h:79`.
const IOCTL_SET_VARIABLE: u32 = 263;
/// How long the firmware may take to answer. Not measured.
const CONTROL_TIMEOUT_MS: u64 = 2000;

/// `brcmfmac/fwil.h:23`.
const IOCTL_SET_SSID: u32 = 26;
/// `brcmfmac/fwil.h:13`.
const IOCTL_UP: u32 = 2;
/// `brcmfmac/fwil.h:18`.
const IOCTL_SET_INFRA: u32 = 20;
/// `brcmfmac/fwil.h:50`.
const IOCTL_SET_PM: u32 = 86;
/// `brcmfmac/fwil.h:69,70,77`, with the times of `brcmfmac/cfg80211.c:82-84`, in milliseconds.
const IOCTL_SCAN_TIMES: [(u32, u32); 3] = [(185, 40), (187, 40), (258, 120)];

/// `brcmfmac/sdio.c:1337`.
const CHANNEL_EVENT: u8 = 1;
/// `brcmfmac/sdio.c:1338`.
const CHANNEL_DATA: u8 = 2;
/// `brcmfmac/bcdc.c:47`, `struct brcmf_proto_bcdc_header` at `:71`: its last byte gives the words
/// after it, as `:321` skips them.
const BCDC_DATA_HEADER: usize = 4;
/// `brcmfmac/bcdc.c:48`.
const BCDC_DATA_VERSION: u8 = 2;
/// `brcmfmac/bcdc.c:50`.
const BCDC_DATA_VERSION_SHIFT: u32 = 4;
/// `include/uapi/linux/if_ether.h:84` `ETH_P_LINK_CTL`.
const ETHERTYPE_LINK_CONTROL: u16 = 0x886C;
/// Offsets in `struct brcmf_event`, `brcmfmac/fweh.h:214-247`, which is packed and big-endian: the
/// protocol of the Ethernet header, the OUI and subtype of Broadcom's header, and its message.
const EVENT_PROTOCOL: usize = 12;
const EVENT_OUI: usize = 19;
const EVENT_SUBTYPE: usize = 22;
const EVENT_MESSAGE: usize = 24;
/// Offsets from that message: its code, status and data length, and its size.
const MESSAGE_FLAGS: usize = 2;
const MESSAGE_CODE: usize = 4;
const MESSAGE_STATUS: usize = 8;
const MESSAGE_REASON: usize = 12;
const MESSAGE_LENGTH: usize = 20;
const MESSAGE_SIZE: usize = 48;
/// `brcmfmac/fweh.h:201`.
const BROADCOM_OUI: [u8; 3] = [0x00, 0x10, 0x18];
/// `brcmfmac/fweh.h:202`.
const SUBTYPE_EVENT: u16 = 1;
/// `brcmfmac/fweh.h:22`.
pub const EVENT_SET_SSID: u32 = 0;
/// `brcmfmac/fweh.h:25`.
pub const EVENT_AUTH: u32 = 3;
/// `brcmfmac/fweh.h:27`.
pub const EVENT_DEAUTH: u32 = 5;
/// `brcmfmac/fweh.h:28`.
pub const EVENT_DEAUTH_IND: u32 = 6;
/// `brcmfmac/fweh.h:29`.
pub const EVENT_ASSOC: u32 = 7;
/// `brcmfmac/fweh.h:33`.
pub const EVENT_DISASSOC: u32 = 11;
/// `brcmfmac/fweh.h:34`.
pub const EVENT_DISASSOC_IND: u32 = 12;
/// `brcmfmac/fweh.h:38`.
pub const EVENT_LINK: u32 = 16;
/// `brcmfmac/fweh.h:85`.
const EVENT_SCAN_RESULT: u32 = 69;
/// Events asked of the firmware.
const EVENTS: [u32; 9] = [
    EVENT_SET_SSID,
    EVENT_AUTH,
    EVENT_DEAUTH,
    EVENT_DEAUTH_IND,
    EVENT_ASSOC,
    EVENT_DISASSOC,
    EVENT_DISASSOC_IND,
    EVENT_LINK,
    EVENT_SCAN_RESULT,
];
/// `brcmfmac/fweh.h:112`: in an event's flags, that the link is up.
const EVENT_FLAG_LINK: u16 = 1;
/// Events kept while waiting for the answer to a command.
const MAX_QUEUED_EVENTS: usize = 8;
/// `brcmfmac/fweh.h:109`: 139 events, in bytes of mask.
const EVENT_MASK_BYTES: usize = 18;
/// `brcmfmac/fweh.h:117`.
pub const STATUS_SUCCESS: u32 = 0;
/// `brcmfmac/fweh.h:125`.
const STATUS_PARTIAL: u32 = 8;

/// `struct brcmf_escan_params_le`, `brcmfmac/fwil_types.h:401-406`, with
/// `struct brcmf_scan_params_le`, `brcmfmac/fwil_types.h:353-392`: its fixed part is 64 bytes.
const SCAN_PARAMS_BYTES: usize = 72;
/// Offset of the BSSID, after the version, action, sync ID and the SSID.
const SCAN_BSSID: usize = 44;
/// Offset of the BSS type.
const SCAN_BSS_TYPE: usize = 50;
/// Offset of the probe count and the three times, each -1 for the firmware's default.
const SCAN_DEFAULTS: core::ops::Range<usize> = 52..68;
/// `brcmfmac/fwil_types.h:69`.
const SCAN_REQUEST_VERSION: u32 = 1;
/// `brcmfmac/cfg80211.h:49`.
const SCAN_ACTION_START: u16 = 1;
/// `brcmfmac/cfg80211.c:1129`: an ID for the request.
const SCAN_SYNC_ID: u16 = 0x1234;
/// `brcmfmac/fwil_types.h:68`.
const BSS_TYPE_ANY: u8 = 2;

/// Size of `struct brcmf_escan_result_le`, `brcmfmac/fwil_types.h:408-414`, before its first entry.
const RESULT_HEADER: usize = 12;
/// Offsets in `struct brcmf_bss_info_le`, `brcmfmac/fwil_types.h:305-339`, not packed. Checked
/// by compiling that struct and asking for them.
const BSS_LENGTH: usize = 4;
const BSS_BSSID: usize = 8;
const BSS_CAPABILITY: usize = 16;
const BSS_SSID_LENGTH: usize = 18;
const BSS_SSID: usize = 19;
const BSS_CHANSPEC: usize = 72;
const BSS_RSSI: usize = 78;
const BSS_N_CAPABLE: usize = 81;
const BSS_CONTROL_CHANNEL: usize = 88;
const BSS_IE_OFFSET: usize = 116;
const BSS_IE_LENGTH: usize = 120;
const BSS_FIXED_SIZE: usize = 128;
/// `include/linux/ieee80211.h:2598`.
const CAPABILITY_PRIVACY: u16 = 1 << 4;
/// `include/linux/ieee80211.h:2824`.
const IE_RSN: u8 = 48;
/// `include/linux/ieee80211.h:2978`.
const IE_VENDOR: u8 = 221;
/// `brcmfmac/cfg80211.c:37-38`: the vendor element of WPA.
const WPA_VENDOR_PREFIX: [u8; 4] = [0x00, 0x50, 0xF2, 0x01];

/// `include/uapi/linux/if_ether.h:88` `ETH_P_PAE`.
pub const ETHERTYPE_EAPOL: u16 = 0x888E;
/// Bytes of an Ethernet header.
const ETHERNET_HEADER: usize = 14;
/// Bytes of the longest EAPOL frame taken: its fixed part and what the handshake's key data needs.
const EAPOL_MAX: usize = 512;
/// How long message 4 gets to leave the chip before its key is installed, or the chip would
/// encrypt it. `brcmfmac/core.c:1491` only waits for the frame to reach the bus, which here it has.
/// This margin is a choice, not measured.
const MESSAGE_4_SETTLE_MS: u64 = 20;

/// `brcm80211/include/brcmu_wifi.h:226`.
const WPA2_AUTH_PSK: u32 = 0x80;
/// `brcm80211/include/brcmu_wifi.h:208`.
const AES_ENABLED: u32 = 4;
/// `brcmfmac/cfg80211.c`, `brcmf_set_auth_type`: open system.
const AUTH_OPEN: u32 = 0;
/// `struct brcmf_ext_join_params_le`, `brcmfmac/fwil_types.h:468`, less its list of channels as
/// `brcmfmac/cfg80211.c` sends it when it names none. Not packed: checked by compiling it.
const JOIN_PARAMS_BYTES: usize = 68;
/// Offsets in it: the scan's type, its four numbers, each -1 for the firmware's default, and the
/// access point.
const JOIN_SCAN_TYPE: usize = 36;
const JOIN_SCAN_DEFAULTS: core::ops::Range<usize> = 40..56;
const JOIN_BSSID: usize = 56;
/// `struct brcmf_join_params`, `brcmfmac/fwil_types.h:447`, the older way to join, and the offset
/// of the access point in it.
const OLD_JOIN_BYTES: usize = 52;
const OLD_JOIN_BSSID: usize = 36;

/// `struct brcmf_wsec_key_le`, `brcmfmac/fwil_types.h:496-513`. Not packed: checked by compiling it.
const KEY_BYTES: usize = 164;
const KEY_INDEX: usize = 0;
const KEY_LENGTH: usize = 4;
const KEY_DATA: usize = 8;
const KEY_ALGORITHM: usize = 112;
const KEY_FLAGS: usize = 116;
const KEY_IV_INITIALIZED: usize = 132;
const KEY_RX_IV_HIGH: usize = 140;
const KEY_RX_IV_LOW: usize = 144;
const KEY_PEER: usize = 156;
/// `brcm80211/include/brcmu_wifi.h:199`.
const KEY_ALGORITHM_AES: u32 = 4;
/// `brcmfmac/fwil_types.h:67`.
const KEY_FLAG_PRIMARY: u32 = 1 << 1;

/// Bytes read back per step when checking what was loaded.
const VERIFY_CHUNK: usize = 2048;
/// Room for the packed NVRAM.
const NVRAM_MAX: usize = 4096;
/// Number of cores this keeps. The table of this chip has fewer.
const MAX_CORES: usize = 16;

const COUNTS_PER_US: u64 = soc::MCT_CLOCK_HZ as u64 / 1_000_000;

/// Why a step failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The PMIC did not take a change.
    Pmic(max77686::Error),
    /// The controller failed before any card was involved.
    Host(sdhci::Error),
    /// The card or the bus failed.
    Sdio(sdio::Error),
    /// The chip's clock control register holds something else: what was written, what was read.
    ClockControl(u8, u8),
    /// The chip's slow clock did not start, with the last value of its clock control register.
    AlpTimeout(u8),
    /// The chip's fast clock did not start, with the last value of its clock control register.
    HtTimeout(u8),
    /// This is not the chip this driver is for: its ID register.
    WrongChip(u32),
    /// The chip's table of cores makes no sense, or has no end.
    BadCoreTable,
    /// The chip lacks this core.
    CoreMissing(u16),
    /// This core should be running after a reset.
    CoreDown(u16),
    /// The RAM has a size this does not believe.
    BadRamSize(u32),
    /// The firmware or NVRAM is empty or does not fit in RAM.
    BadImage,
    /// What was read back from RAM differs from what was written: where, and what was read.
    VerifyFailed(u32, u8),
    /// A frame's header is not valid: its first four bytes.
    BadFrame(u32),
    /// A frame or message that does not fit.
    BadLength,
    /// The firmware did not answer.
    NoAnswer,
    /// The firmware reports it has stopped: the mailbox value.
    FirmwareHalted(u32),
    /// The firmware refused a command, with its error code.
    Firmware(i32),
    /// A scan did not finish in the time given.
    ScanTimeout,
    /// Joining did not finish in the time given.
    JoinTimeout,
    /// The firmware could not join: its status and reason.
    JoinFailed {
        /// The status, from `BRCMF_E_STATUS_*`.
        status: u32,
        /// The reason, from `BRCMF_E_REASON_*`.
        reason: u32,
    },
    /// The network ended the connection, or the link went down.
    Disconnected {
        /// The event that told.
        event: u32,
        /// The reason, from IEEE Std 802.11. 15 is a handshake that timed out, which is how a
        /// wrong passphrase looks from here.
        reason: u32,
    },
    /// A handshake cannot go on.
    Wpa(wpa::Error),
}

impl From<max77686::Error> for Error {
    fn from(error: max77686::Error) -> Self {
        Self::Pmic(error)
    }
}

impl From<sdhci::Error> for Error {
    fn from(error: sdhci::Error) -> Self {
        Self::Host(error)
    }
}

impl From<sdio::Error> for Error {
    fn from(error: sdio::Error) -> Self {
        Self::Sdio(error)
    }
}

/// The power state of the chip as it was found, for reports. Raw PMIC values.
#[derive(Clone, Copy, Debug)]
pub struct PowerState {
    /// Control register of LDO22.
    pub ldo: u8,
    /// Enables of the 32 kHz outputs.
    pub clocks_32k: u8,
    /// Whether the supply's enable pin is high.
    pub ldo_enable: bool,
    /// Whether the chip's enable pin is high.
    pub wlan_enable: bool,
}

/// The contents of the chip's ID register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipId(u32);

impl ChipId {
    /// The raw register.
    pub const fn raw(&self) -> u32 {
        self.0
    }

    /// The chip's number.
    pub const fn chip(&self) -> u16 {
        (self.0 & CID_ID_MASK) as u16
    }

    /// Its revision.
    pub const fn revision(&self) -> u8 {
        ((self.0 >> CID_REVISION_SHIFT) & CID_REVISION_MASK) as u8
    }

    /// The type of its backplane: `brcmfmac/chip.c:21-22`, where 1 is AXI.
    pub const fn backplane_type(&self) -> u8 {
        ((self.0 >> CID_TYPE_SHIFT) & CID_TYPE_MASK) as u8
    }
}

/// One core on the chip's backplane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Core {
    /// Its kind.
    pub id: u16,
    /// Its revision.
    pub revision: u8,
    /// Address of its registers.
    pub base: u32,
    /// Address of its wrapper, which resets it and gates its clock.
    pub wrapper: u32,
}

/// What [`Bcm4334::load_firmware`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadInfo {
    /// Bytes of NVRAM after packing.
    pub nvram_bytes: usize,
    /// Where the NVRAM went.
    pub nvram_address: u32,
}

/// Raw registers, for reports when the firmware does not answer.
#[derive(Clone, Copy, Debug)]
pub struct Diagnostics {
    /// The SDIO core's interrupt status.
    pub int_status: u32,
    /// What the firmware left in the mailbox.
    pub mailbox: u32,
    /// The chip's clock control register.
    pub clock: u8,
    /// The last word of RAM. Once running, firmware leaves there the address of its shared state.
    pub last_ram_word: u32,
}

/// What a network asks of those who join it, from its beacon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// Nothing.
    Open,
    /// Encryption without WPA, so WEP.
    Wep,
    /// WPA.
    Wpa,
    /// WPA2 or later.
    Wpa2,
}

/// A network seen in a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bss {
    /// The access point's address.
    pub bssid: [u8; 6],
    /// The network's name, in its first [`ssid_length`](Self::ssid_length) bytes.
    pub ssid: [u8; 32],
    /// Bytes of the name. 0 is a hidden network.
    pub ssid_length: u8,
    /// Signal strength, in dBm.
    pub rssi: i16,
    /// The control channel.
    pub channel: u8,
    /// What it asks for.
    pub security: Security,
}

/// An event from the firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    /// What happened, from `BRCMF_E_*`.
    pub code: u32,
    /// How it went.
    pub status: u32,
    /// Why, in terms of the event.
    pub reason: u32,
    /// Its flags.
    pub flags: u16,
}

/// What [`Bcm4334::receive`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
    /// Nothing is waiting.
    Nothing,
    /// An event.
    Event(Event),
    /// An Ethernet frame, whose bytes are valid until the next call.
    Ethernet(&'a [u8]),
}

/// What happened while serving a connection, to report it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    /// The firmware told of something.
    Event(Event),
    /// A message of the handshake arrived, and was answered.
    Handshake(wpa::Message),
    /// A message of the handshake was ignored.
    Ignored(wpa::Error),
}

/// Reads an entry of a scan's results, and finds the network's security element, with its header.
fn parse_bss(bss: &[u8]) -> Option<(Bss, &[u8])> {
    let le16 = |at: usize| u16::from_le_bytes([bss[at], bss[at + 1]]);
    let le32 = |at: usize| u32::from_le_bytes([bss[at], bss[at + 1], bss[at + 2], bss[at + 3]]);
    if bss.len() < BSS_FIXED_SIZE {
        return None;
    }
    let ssid_length = bss[BSS_SSID_LENGTH].min(32);
    let mut ssid = [0; 32];
    ssid[..usize::from(ssid_length)].copy_from_slice(&bss[BSS_SSID..BSS_SSID + usize::from(ssid_length)]);
    let mut bssid = [0; 6];
    bssid.copy_from_slice(&bss[BSS_BSSID..BSS_BSSID + 6]);

    // The low byte of the channel specification is the centre of a wide channel, which is why
    // an 802.11n network has a field for its control channel.
    let channel = if bss[BSS_N_CAPABLE] != 0 {
        bss[BSS_CONTROL_CHANNEL]
    } else {
        le16(BSS_CHANSPEC) as u8
    };

    let mut security =
        if le16(BSS_CAPABILITY) & CAPABILITY_PRIVACY != 0 { Security::Wep } else { Security::Open };
    let start = usize::from(le16(BSS_IE_OFFSET));
    let elements = bss.get(start..start.checked_add(le32(BSS_IE_LENGTH) as usize)?)?;
    let mut rest = elements;
    let mut rsn: &[u8] = &[];
    while let [id, length, tail @ ..] = rest {
        let Some(body) = tail.get(..usize::from(*length)) else { break };
        if *id == IE_RSN {
            security = Security::Wpa2;
            rsn = &rest[..2 + body.len()];
        } else if *id == IE_VENDOR && body.starts_with(&WPA_VENDOR_PREFIX) && security != Security::Wpa2 {
            security = Security::Wpa;
        }
        rest = &tail[usize::from(*length)..];
    }

    let found = Bss {
        bssid,
        ssid,
        ssid_length,
        rssi: le16(BSS_RSSI) as i16,
        channel,
        security,
    };
    Some((found, rsn))
}

/// Reads the chip's power state, without changing it.
pub fn power_state(pmic: Max77686) -> Result<PowerState, Error> {
    Ok(PowerState {
        ldo: pmic.ldo(LDO_WIFI.0)?.raw(),
        clocks_32k: pmic.clocks_32k()?,
        ldo_enable: LDO_ENABLE.read(),
        wlan_enable: WLAN_ENABLE.read(),
    })
}

/// Powers the chip up from reset, whatever state it was in, and configures the pins of its bus.
///
/// Its supplies stay on afterwards. So does its enable, until a later call resets it. The PMIC
/// keeps its state over a restart of the board.
pub fn power_on(pmic: Max77686, mct: Mct) -> Result<(), Error> {
    if pmic.ldo(LDO_WIFI.0)?.mode() == 0 {
        pmic.enable_ldo(LDO_WIFI.0, LDO_WIFI.1)?;
        delay_ms(mct, RAIL_SETTLE_MS);
    }
    // `exynos4412-mehmet.dts:1537,945` give the 32 kHz clock of WiFi and of Bluetooth, which are in
    // one package.
    pmic.enable_clock_32k(Clock32k::Pmic)?;
    pmic.enable_clock_32k(Clock32k::Cp)?;

    WLAN_ENABLE.set_pull(Pull::None);
    WLAN_ENABLE.set_drive(Drive::Level4);
    WLAN_ENABLE.set_output(false);
    delay_ms(mct, RESET_MS);

    LDO_ENABLE.set_pull(Pull::None);
    LDO_ENABLE.set_drive(Drive::Level1);
    LDO_ENABLE.set_output(true);
    delay_ms(mct, LDO_ENABLE_SETTLE_MS);

    for (pin, pull) in [(SD_CLOCK, Pull::None), (SD_COMMAND, Pull::Up)] {
        pin.set_pull(pull);
        pin.set_drive(Drive::Level1);
        pin.set_function(SD_FUNCTION);
    }
    for pin in SD_DATA {
        pin.set_pull(Pull::Up);
        pin.set_drive(Drive::Level1);
        pin.set_function(SD_FUNCTION);
    }

    WLAN_ENABLE.write(true);
    delay_ms(mct, AFTER_RESET_MS);
    Ok(())
}

fn delay_us(mct: Mct, us: u64) {
    let start = mct.count();
    while mct.count() - start < us * COUNTS_PER_US {
        core::hint::spin_loop();
    }
}

/// The chip, started on its SDIO bus.
#[derive(Debug)]
pub struct Bcm4334 {
    card: Sdio,
    mct: Mct,
    window: u32,
    cores: [Core; MAX_CORES],
    core_count: usize,
    ram_size: u32,
    pmu_revision: u8,
    frames_pending: bool,
    tx_sequence: u8,
    tx_limit: u8,
    request_id: u16,
    frame: [u8; FRAME_SIZE],
    queued_events: [Option<Event>; MAX_QUEUED_EVENTS],
    /// Whether this connection has reached its access point.
    associated: bool,
}

impl Bcm4334 {
    /// Starts the card on a chip just out of reset: the bus at 1 bit and 400 kHz, then at 4 bits
    /// and the board's data rate, with function 1 on. Returns also the rate.
    pub fn start(host: Sdhci, mct: Mct) -> Result<(Self, u32), Error> {
        host.set_clock(IDENTIFY_HZ)?;
        let card = Sdio::start(host, mct)?;
        card.set_bus_4bit()?;
        let rate = host.set_clock(DATA_HZ)?;
        card.set_block_size(FUNCTION1, FUNCTION1_BLOCK_SIZE)?;
        card.set_block_size(FUNCTION2, FUNCTION2_BLOCK_SIZE)?;
        card.enable_function(mct, FUNCTION1, FUNCTION1_READY_MS)?;
        let chip = Self {
            card,
            mct,
            // Not an address the window can hold: the first access always sets it.
            window: u32::MAX,
            cores: [Core::default(); MAX_CORES],
            core_count: 0,
            ram_size: 0,
            pmu_revision: 0,
            frames_pending: false,
            tx_sequence: FIRST_TX_SEQUENCE,
            tx_limit: 0,
            request_id: 0,
            frame: [0; FRAME_SIZE],
            queued_events: [None; MAX_QUEUED_EVENTS],
            associated: false,
        };
        Ok((chip, rate))
    }

    /// The card under this chip.
    pub const fn card(&self) -> Sdio {
        self.card
    }

    /// The cores found by [`attach`](Self::attach).
    pub fn cores(&self) -> &[Core] {
        &self.cores[..self.core_count]
    }

    /// Bytes of RAM, found by [`attach`](Self::attach).
    pub const fn ram_size(&self) -> u32 {
        self.ram_size
    }

    /// Revision of the power management unit, found by [`attach`](Self::attach).
    pub const fn pmu_revision(&self) -> u8 {
        self.pmu_revision
    }

    fn set_window(&mut self, address: u32) -> Result<(), Error> {
        let window = address & WINDOW_MASK;
        if window != self.window {
            for (i, byte) in window.to_le_bytes()[1..].iter().enumerate() {
                self.card.write_byte(FUNCTION1, REG_WINDOW_LOW + i as u32, *byte)?;
            }
            self.window = window;
        }
        Ok(())
    }

    /// Reads a 32-bit register of the chip's backplane.
    pub fn read_register(&mut self, address: u32) -> Result<u32, Error> {
        self.set_window(address)?;
        let offset = (address & WINDOW_OFFSET_MASK) | WINDOW_ACCESS_32BIT;
        let mut bytes = [0; 4];
        self.card.read(FUNCTION1, offset, true, usize::from(FUNCTION1_BLOCK_SIZE), &mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Writes a 32-bit register of the chip's backplane.
    pub fn write_register(&mut self, address: u32, value: u32) -> Result<(), Error> {
        self.set_window(address)?;
        let offset = (address & WINDOW_OFFSET_MASK) | WINDOW_ACCESS_32BIT;
        let bytes = value.to_le_bytes();
        self.card.write(FUNCTION1, offset, true, usize::from(FUNCTION1_BLOCK_SIZE), &bytes)?;
        Ok(())
    }

    /// The part of `length` from `address` that fits in one window, and its offset there.
    fn window_chunk(&mut self, address: u32, length: usize) -> Result<(usize, u32), Error> {
        self.set_window(address)?;
        let offset = address & WINDOW_OFFSET_MASK;
        let chunk = length.min((WINDOW_SIZE - offset) as usize);
        Ok((chunk, offset | WINDOW_ACCESS_32BIT))
    }

    /// Writes `data`, a whole number of words, to the chip's memory.
    fn write_memory(&mut self, mut address: u32, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            let (chunk, offset) = self.window_chunk(address, data.len())?;
            let block = usize::from(FUNCTION1_BLOCK_SIZE);
            self.card.write(FUNCTION1, offset, true, block, &data[..chunk])?;
            data = &data[chunk..];
            address += chunk as u32;
        }
        Ok(())
    }

    /// Reads `data`, a whole number of words, from the chip's memory.
    fn read_memory(&mut self, mut address: u32, mut data: &mut [u8]) -> Result<(), Error> {
        while !data.is_empty() {
            let (chunk, offset) = self.window_chunk(address, data.len())?;
            let block = usize::from(FUNCTION1_BLOCK_SIZE);
            let (now, rest) = data.split_at_mut(chunk);
            self.card.read(FUNCTION1, offset, true, block, now)?;
            data = rest;
            address += chunk as u32;
        }
        Ok(())
    }

    /// Writes `data` of any length to the chip's memory, padding it to a word with zeros as
    /// `brcmfmac/bcmsdh.c:323` does, and checks it by reading it back.
    fn load(&mut self, address: u32, data: &[u8]) -> Result<(), Error> {
        let whole = data.len() & !3;
        self.write_memory(address, &data[..whole])?;
        let mut tail = [0; 4];
        let extra = data.len() - whole;
        if extra != 0 {
            tail[..extra].copy_from_slice(&data[whole..]);
            self.write_memory(address + whole as u32, &tail)?;
        }

        let mut read = [0; VERIFY_CHUNK];
        let mut at = 0;
        while at < whole {
            let now = (whole - at).min(VERIFY_CHUNK);
            self.read_memory(address + at as u32, &mut read[..now])?;
            if let Some(i) = (0..now).find(|&i| read[i] != data[at + i]) {
                return Err(Error::VerifyFailed(address + (at + i) as u32, read[i]));
            }
            at += now;
        }
        if extra != 0 {
            let mut back = [0; 4];
            self.read_memory(address + whole as u32, &mut back)?;
            if back != tail {
                return Err(Error::VerifyFailed(address + whole as u32, back[0]));
            }
        }
        Ok(())
    }

    /// Reads the chip's ID register.
    pub fn chip_id(&mut self) -> Result<ChipId, Error> {
        Ok(ChipId(self.read_register(ENUM_BASE)?))
    }

    /// Starts the chip's slow clock, and keeps it on. Returns its clock control register.
    fn start_slow_clock(&self) -> Result<u8, Error> {
        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, CLOCK_REQUEST_ALP)?;
        let mut value = self.card.read_byte(FUNCTION1, REG_CHIP_CLOCK)?;
        if value & !CLOCK_AVAILABLE_BITS != CLOCK_REQUEST_ALP {
            return Err(Error::ClockControl(CLOCK_REQUEST_ALP, value));
        }
        let begin = self.mct.count();
        while value & CLOCK_ALP_AVAILABLE == 0 {
            if self.mct.count() - begin > CLOCK_TIMEOUT_MS * 1000 * COUNTS_PER_US {
                return Err(Error::AlpTimeout(value));
            }
            value = self.card.read_byte(FUNCTION1, REG_CHIP_CLOCK)?;
        }
        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, CLOCK_FORCE_ALP)?;
        delay_us(self.mct, CLOCK_FORCE_SETTLE_US);
        self.card.write_byte(FUNCTION1, REG_PULL_UPS, 0)?;
        Ok(self.card.read_byte(FUNCTION1, REG_CHIP_CLOCK)?)
    }

    fn next_descriptor(&mut self, at: &mut u32) -> Result<(u32, u32), Error> {
        let value = self.read_register(*at)?;
        *at += 4;
        let mut kind = value & DESC_TYPE_MASK;
        if kind & !DESC_SIZE_GT32 == DESC_ADDRESS {
            kind = DESC_ADDRESS;
        }
        Ok((value, kind))
    }

    /// Finds the registers and wrapper of the component just read. `None` if it has neither.
    fn core_addresses(&mut self, at: &mut u32) -> Result<Option<(u32, u32)>, Error> {
        let (_, first) = self.next_descriptor(at)?;
        let wrapper_type = match first {
            DESC_MASTER_PORT => SLAVE_TYPE_MWRAP,
            DESC_ADDRESS => {
                *at -= 4;
                SLAVE_TYPE_SWRAP
            }
            _ => {
                *at -= 4;
                return Ok(None);
            }
        };

        let (mut base, mut wrapper) = (0, 0);
        for _ in 0..EROM_MAX_ENTRIES {
            let (value, kind) = self.next_descriptor(at)?;
            match kind {
                DESC_END => {
                    *at -= 4;
                    return Ok(None);
                }
                DESC_COMPONENT => {
                    *at -= 4;
                    return Ok(Some((base, wrapper)));
                }
                DESC_ADDRESS => {}
                _ => continue,
            }
            if value & DESC_SIZE_GT32 != 0 {
                self.next_descriptor(at)?;
            }
            let size = (value >> SLAVE_SIZE_SHIFT) & SLAVE_SIZE_MASK;
            if size == SLAVE_SIZE_DESC {
                let (size_entry, _) = self.next_descriptor(at)?;
                if size_entry & DESC_SIZE_GT32 != 0 {
                    self.next_descriptor(at)?;
                }
            }
            if size <= SLAVE_SIZE_8K {
                let slave = (value >> SLAVE_TYPE_SHIFT) & SLAVE_TYPE_MASK;
                if base == 0 && slave == SLAVE_TYPE_SLAVE {
                    base = value & SLAVE_BASE_MASK;
                }
                if wrapper == 0 && slave == wrapper_type {
                    wrapper = value & SLAVE_BASE_MASK;
                }
            }
            if base != 0 && wrapper != 0 {
                return Ok(Some((base, wrapper)));
            }
        }
        Err(Error::BadCoreTable)
    }

    fn scan_cores(&mut self) -> Result<(), Error> {
        let mut at = self.read_register(ENUM_BASE + CC_EROM_POINTER)?;
        self.core_count = 0;
        for _ in 0..EROM_MAX_ENTRIES {
            let (value, kind) = self.next_descriptor(&mut at)?;
            if kind == DESC_END {
                return Ok(());
            }
            if value & DESC_VALID == 0 || kind != DESC_COMPONENT {
                continue;
            }
            let id = ((value >> COMP_PART_SHIFT) & COMP_PART_MASK) as u16;
            let (second, second_kind) = self.next_descriptor(&mut at)?;
            if second_kind != DESC_COMPONENT {
                return Err(Error::BadCoreTable);
            }
            let wrappers = ((second >> COMP_MWRAP_SHIFT) & COMP_WRAP_MASK)
                + ((second >> COMP_SWRAP_SHIFT) & COMP_WRAP_MASK);
            if wrappers == 0 && id != CORE_PMU && id != CORE_GCI {
                continue;
            }
            let Some((base, wrapper)) = self.core_addresses(&mut at)? else {
                continue;
            };
            let revision = (second >> COMP_REVISION_SHIFT) as u8;
            *self.cores.get_mut(self.core_count).ok_or(Error::BadCoreTable)? =
                Core { id, revision, base, wrapper };
            self.core_count += 1;
        }
        Err(Error::BadCoreTable)
    }

    fn core(&self, id: u16) -> Result<Core, Error> {
        self.cores().iter().find(|core| core.id == id).copied().ok_or(Error::CoreMissing(id))
    }

    fn core_is_up(&mut self, core: Core) -> Result<bool, Error> {
        let control = self.read_register(core.wrapper + WRAP_IOCTL)?;
        let reset = self.read_register(core.wrapper + WRAP_RESET)?;
        Ok(control & (IOCTL_FORCE_GATED | IOCTL_CLOCK) == IOCTL_CLOCK && reset & RESET_ASSERT == 0)
    }

    fn core_disable(&mut self, core: Core, pre_reset: u32, in_reset: u32) -> Result<(), Error> {
        let wrapper = core.wrapper;
        if self.read_register(wrapper + WRAP_RESET)? & RESET_ASSERT == 0 {
            self.write_register(wrapper + WRAP_IOCTL, pre_reset | IOCTL_FORCE_GATED | IOCTL_CLOCK)?;
            self.read_register(wrapper + WRAP_IOCTL)?;
            self.write_register(wrapper + WRAP_RESET, RESET_ASSERT)?;
            delay_us(self.mct, RESET_ASSERT_WAIT_US);
        }
        self.write_register(wrapper + WRAP_IOCTL, in_reset | IOCTL_FORCE_GATED | IOCTL_CLOCK)?;
        self.read_register(wrapper + WRAP_IOCTL)?;
        Ok(())
    }

    fn core_reset(&mut self, core: Core, pre_reset: u32, in_reset: u32, after: u32) -> Result<(), Error> {
        self.core_disable(core, pre_reset, in_reset)?;
        for _ in 0..RESET_RELEASE_TRIES {
            if self.read_register(core.wrapper + WRAP_RESET)? & RESET_ASSERT == 0 {
                break;
            }
            self.write_register(core.wrapper + WRAP_RESET, 0)?;
            delay_us(self.mct, RESET_RELEASE_WAIT_US);
        }
        self.write_register(core.wrapper + WRAP_IOCTL, after | IOCTL_CLOCK)?;
        self.read_register(core.wrapper + WRAP_IOCTL)?;
        Ok(())
    }

    fn read_ram_size(&mut self, ram: Core) -> Result<u32, Error> {
        let info = self.read_register(ram.base + RAM_CORE_INFO)?;
        let banks = (info >> RAM_BANKS_SHIFT) & RAM_BANKS_MASK;
        let size = if ram.revision <= RAM_REV_SIMPLE_MAX || ram.revision == RAM_REV_SIMPLE_ALSO {
            let bank = 1u32 << ((info & RAM_BANK_SIZE_MASK) + RAM_BANK_SIZE_BASE);
            match (info >> RAM_LAST_SIZE_SHIFT) & RAM_LAST_SIZE_MASK {
                0 => banks * bank,
                last => banks.saturating_sub(1) * bank + (1 << (last - 1 + RAM_BANK_SIZE_BASE)),
            }
        } else {
            let banks = if ram.revision >= RAM_REV_WIDE {
                (info >> RAM_BANKS_SHIFT) & RAM_BANKS_MASK_EXT
            } else {
                banks
            };
            let mut total = 0;
            for bank in 0..banks {
                // The memory type in the index stays 0, RAM: `brcmfmac/chip.c:151`.
                self.write_register(ram.base + RAM_BANK_INDEX, bank)?;
                let bank_info = self.read_register(ram.base + RAM_BANK_INFO)?;
                total += ((bank_info & RAM_BANK_INFO_SIZE_MASK) + 1) * RAM_BANK_INFO_SIZE_UNIT;
            }
            total
        };
        if size == 0 || size > RAM_MAX_SIZE {
            return Err(Error::BadRamSize(size));
        }
        Ok(size)
    }

    /// Takes the chip's processor out of action and finds out what the chip holds, ready for
    /// [`load_firmware`](Self::load_firmware). Returns its ID register.
    pub fn attach(&mut self) -> Result<ChipId, Error> {
        self.start_slow_clock()?;
        let id = self.chip_id()?;
        if id.chip() != CHIP_ID_4334 || id.backplane_type() != BACKPLANE_AI {
            return Err(Error::WrongChip(id.raw()));
        }
        self.scan_cores()?;

        let common = self.core(CORE_COMMON)?;
        let capabilities = self.read_register(common.base + CC_CAPABILITIES)?;
        let capabilities_ext = self.read_register(common.base + CC_CAPABILITIES_EXT)?;
        let separate_pmu =
            common.revision >= CC_REV_SEPARATE_PMU && capabilities_ext & CC_CAP_EXT_AOB != 0;
        let pmu = if separate_pmu { self.core(CORE_PMU).unwrap_or(common) } else { common };
        if capabilities & CC_CAP_PMU != 0 {
            let value = self.read_register(pmu.base + CC_PMU_CAPABILITIES)?;
            self.pmu_revision = (value & PMU_CAP_REV_MASK) as u8;
        }

        let arm = self.core(CORE_ARM_CM3)?;
        self.core_disable(arm, 0, 0)?;
        let radio = self.core(CORE_80211)?;
        self.core_reset(radio, D11_PHY_RESET | D11_PHY_CLOCK, D11_PHY_CLOCK, D11_PHY_CLOCK)?;
        let ram = self.core(CORE_RAM)?;
        self.core_reset(ram, 0, 0, 0)?;
        self.ram_size = self.read_ram_size(ram)?;

        let sdio = self.core(CORE_SDIO)?;
        if sdio.revision >= SDIO_CORE_REV_KEEP_ON {
            let sleep = self.card.read_byte(FUNCTION1, REG_SLEEP)?;
            if sleep & SLEEP_KEEP_ON == 0 {
                self.card.write_byte(FUNCTION1, REG_SLEEP, sleep | SLEEP_KEEP_ON)?;
            }
        }
        let control = self.card.read_byte(0, CCCR_BRCM_CARD_CONTROL)?;
        self.card.write_byte(0, CCCR_BRCM_CARD_CONTROL, control | CARD_CONTROL_WLAN_RESET)?;
        let value = self.read_register(pmu.base + CC_PMU_CONTROL)?;
        self.write_register(pmu.base + CC_PMU_CONTROL, value | PMU_CONTROL_RESOURCE_RELOAD)?;

        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, 0)?;
        Ok(id)
    }

    fn wait_clock(&self, available: u8) -> Result<u8, u8> {
        let mut value = 0;
        let begin = self.mct.count();
        while self.mct.count() - begin < CLOCK_TIMEOUT_MS * 1000 * COUNTS_PER_US {
            value = self.card.read_byte(FUNCTION1, REG_CHIP_CLOCK).map_err(|_| value)?;
            if value & available != 0 {
                return Ok(value);
            }
        }
        Err(value)
    }

    /// Writes the firmware and NVRAM to the chip's RAM and checks them. `nvram_text` is the text
    /// of the NVRAM file. Call [`attach`](Self::attach) first.
    pub fn load_firmware(&mut self, code: &[u8], nvram_text: &[u8]) -> Result<LoadInfo, Error> {
        let mut packed = [0; NVRAM_MAX];
        let nvram_bytes = nvram::pack(nvram_text, &mut packed).ok_or(Error::BadImage)?;
        if code.is_empty() || code.len() + nvram_bytes > self.ram_size as usize {
            return Err(Error::BadImage);
        }

        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, CLOCK_ALP_ONLY)?;
        self.wait_clock(CLOCK_ALP_AVAILABLE).map_err(Error::AlpTimeout)?;

        // `brcmf_chip_get_raminfo` leaves the base of RAM at 0 on a chip with this processor.
        self.load(0, code)?;
        let nvram_address = self.ram_size - nvram_bytes as u32;
        self.load(nvram_address, &packed[..nvram_bytes])?;
        Ok(LoadInfo { nvram_bytes, nvram_address })
    }

    /// Runs what [`load_firmware`](Self::load_firmware) loaded, and opens the frame channel.
    /// The clock stays forced on from here.
    pub fn start_firmware(&mut self) -> Result<(), Error> {
        let ram = self.core(CORE_RAM)?;
        if !self.core_is_up(ram)? {
            return Err(Error::CoreDown(CORE_RAM));
        }
        let sdio = self.core(CORE_SDIO)?;
        self.write_register(sdio.base + SDIO_INT_STATUS, u32::MAX)?;
        let arm = self.core(CORE_ARM_CM3)?;
        self.core_reset(arm, 0, 0, 0)?;

        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, 0)?;
        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, CLOCK_REQUEST_HT)?;
        let clock = self.wait_clock(CLOCK_HT_AVAILABLE).map_err(Error::HtTimeout)?;
        self.card.write_byte(FUNCTION1, REG_CHIP_CLOCK, clock | CLOCK_FORCE_HT)?;

        self.write_register(sdio.base + SDIO_TO_CHIP_MAILBOX_DATA, MAILBOX_PROTOCOL_VERSION)?;
        self.card.enable_function(self.mct, FUNCTION2, FUNCTION2_READY_MS)?;
        self.write_register(sdio.base + SDIO_HOST_INT_MASK, HOST_INT_MASK)?;
        self.card.write_byte(FUNCTION1, REG_WATERMARK, DEFAULT_WATERMARK)?;
        Ok(())
    }

    /// Raw registers, for when the firmware does not answer.
    pub fn diagnostics(&mut self) -> Result<Diagnostics, Error> {
        let sdio = self.core(CORE_SDIO)?;
        Ok(Diagnostics {
            int_status: self.read_register(sdio.base + SDIO_INT_STATUS)?,
            mailbox: self.read_register(sdio.base + SDIO_TO_HOST_MAILBOX_DATA)?,
            clock: self.card.read_byte(FUNCTION1, REG_CHIP_CLOCK)?,
            last_ram_word: self.read_register(self.ram_size - 4)?,
        })
    }

    /// The address that function 2 transfers use: `brcmf_sdiod_recv_pkt` in `brcmfmac/bcmsdh.c`.
    fn frame_address(&mut self) -> Result<u32, Error> {
        self.set_window(ENUM_BASE)?;
        Ok((ENUM_BASE & WINDOW_OFFSET_MASK) | WINDOW_ACCESS_32BIT)
    }

    /// Looks for a frame from the firmware. The frame ends up in `self.frame`, and this returns
    /// its channel, and where its contents start and end.
    fn receive_frame(&mut self) -> Result<Option<(u8, usize, usize)>, Error> {
        if !self.frames_pending {
            let sdio = self.core(CORE_SDIO)?;
            let status = self.read_register(sdio.base + SDIO_INT_STATUS)? & HOST_INT_MASK;
            if status != 0 {
                self.write_register(sdio.base + SDIO_INT_STATUS, status)?;
            }
            if status & INT_MAILBOX != 0 {
                let data = self.read_register(sdio.base + SDIO_TO_HOST_MAILBOX_DATA)?;
                self.write_register(sdio.base + SDIO_TO_CHIP_MAILBOX, MAILBOX_ACK)?;
                if data & MAILBOX_FIRMWARE_HALTED != 0 {
                    return Err(Error::FirmwareHalted(data));
                }
            }
            if status & INT_FRAME == 0 {
                return Ok(None);
            }
            self.frames_pending = true;
        }

        let address = self.frame_address()?;
        let block = usize::from(FUNCTION2_BLOCK_SIZE);
        self.card.read(FUNCTION2, address, false, block, &mut self.frame[..FIRST_READ])?;

        let word = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let hardware = word(&self.frame[0..]);
        let length = (hardware & 0xFFFF) as usize;
        let check = hardware >> 16;
        if length == 0 && check == 0 {
            self.frames_pending = false;
            return Ok(None);
        }
        if (length as u32 ^ check) != 0xFFFF || !(HEADER_LENGTH..=FRAME_SIZE).contains(&length) {
            return Err(Error::BadFrame(hardware));
        }
        let software = word(&self.frame[4..]);
        let channel = ((software >> CHANNEL_SHIFT) & CHANNEL_MASK) as u8;
        let offset = (software >> DATA_OFFSET_SHIFT) as usize;
        if !(HEADER_LENGTH..=length).contains(&offset) {
            return Err(Error::BadFrame(hardware));
        }
        self.tx_limit = (word(&self.frame[8..]) >> WINDOW_SEQUENCE_SHIFT) as u8;

        if length > FIRST_READ {
            let rest = (length - FIRST_READ).next_multiple_of(4);
            let end = FIRST_READ + rest;
            let body = self.frame.get_mut(FIRST_READ..end).ok_or(Error::BadLength)?;
            self.card.read(FUNCTION2, address, false, block, body)?;
        }
        Ok(Some((channel, offset, length)))
    }

    /// Whether the firmware can take another frame: what `data_ok` in `brcmfmac/sdio.c:675`
    /// checks, from the limit it gave in its last frame.
    fn can_send(&self) -> bool {
        let room = self.tx_limit.wrapping_sub(self.tx_sequence);
        room != 0 && room & 0x80 == 0
    }

    /// Waits until the firmware can take a frame. Events that come meanwhile are kept. Network
    /// frames are dropped: this has nowhere to put them, and their senders send them again.
    fn wait_to_send(&mut self, begin: u64, timeout: u64) -> Result<(), Error> {
        while !self.can_send() {
            if let Some((channel, offset, length)) = self.receive_frame()? {
                self.keep_event(channel, offset, length);
            }
            if self.mct.count() - begin > timeout {
                return Err(Error::NoAnswer);
            }
        }
        Ok(())
    }

    /// Keeps an event seen while waiting for something else. If too many come, the oldest goes.
    /// Scan results are not kept, since each is a large frame of little value alone.
    fn keep_event(&mut self, channel: u8, offset: usize, length: usize) {
        if channel != CHANNEL_EVENT {
            return;
        }
        let Some((event, _, _)) = self.parse_event(offset, length) else {
            return;
        };
        if event.code == EVENT_SCAN_RESULT {
            return;
        }
        let free = self.queued_events.iter().position(Option::is_none);
        let slot = free.unwrap_or_else(|| {
            self.queued_events.copy_within(1.., 0);
            MAX_QUEUED_EVENTS - 1
        });
        self.queued_events[slot] = Some(event);
    }

    /// Sends a command to the firmware, and waits for its answer. `buffer` holds the input and
    /// gets the output. Returns the length of the output in it.
    pub fn ioctl(&mut self, command: u32, set: bool, buffer: &mut [u8]) -> Result<usize, Error> {
        let message = (BCDC_LENGTH + buffer.len()).min(CONTROL_MESSAGE_MAX);
        let contents = message - BCDC_LENGTH;
        let total = HEADER_LENGTH + message;
        let padded = total.next_multiple_of(4);
        if padded > FRAME_SIZE {
            return Err(Error::BadLength);
        }

        let begin = self.mct.count();
        let timeout = CONTROL_TIMEOUT_MS * 1000 * COUNTS_PER_US;
        self.wait_to_send(begin, timeout)?;

        self.request_id = self.request_id.wrapping_add(1);
        let id = self.request_id;
        let flags = (u32::from(id) << BCDC_ID_SHIFT) | if set { BCDC_SET } else { 0 };
        let check = !(total as u16);
        let sequence = self.tx_sequence;
        let frame = &mut self.frame;
        frame[..padded].fill(0);
        frame[0..2].copy_from_slice(&(total as u16).to_le_bytes());
        frame[2..4].copy_from_slice(&check.to_le_bytes());
        let software = u32::from(sequence) | ((HEADER_LENGTH as u32) << DATA_OFFSET_SHIFT);
        frame[4..8].copy_from_slice(&software.to_le_bytes());
        let bcdc = &mut frame[HEADER_LENGTH..];
        bcdc[0..4].copy_from_slice(&command.to_le_bytes());
        bcdc[4..8].copy_from_slice(&(buffer.len() as u32).to_le_bytes());
        bcdc[8..12].copy_from_slice(&flags.to_le_bytes());
        bcdc[BCDC_LENGTH..BCDC_LENGTH + contents].copy_from_slice(&buffer[..contents]);

        let address = self.frame_address()?;
        let block = usize::from(FUNCTION2_BLOCK_SIZE);
        self.card.write(FUNCTION2, address, false, block, &self.frame[..padded])?;
        self.tx_sequence = sequence.wrapping_add(1);

        let begin = self.mct.count();
        while self.mct.count() - begin < timeout {
            let Some((channel, offset, length)) = self.receive_frame()? else {
                continue;
            };
            if channel != CHANNEL_CONTROL || length < offset + BCDC_LENGTH {
                self.keep_event(channel, offset, length);
                continue;
            }
            let bcdc = &self.frame[offset..length];
            let word = |at: usize| u32::from_le_bytes([bcdc[at], bcdc[at + 1], bcdc[at + 2], bcdc[at + 3]]);
            let flags = word(8);
            if flags >> BCDC_ID_SHIFT != u32::from(id) {
                continue;
            }
            if flags & BCDC_ERROR != 0 {
                return Err(Error::Firmware(word(12) as i32));
            }
            let output = &bcdc[BCDC_LENGTH..];
            let copied = output.len().min(buffer.len());
            buffer[..copied].copy_from_slice(&output[..copied]);
            return Ok(copied);
        }
        Err(Error::NoAnswer)
    }

    /// Reads one of the firmware's variables into `output`.
    pub fn get_variable(&mut self, name: &str, output: &mut [u8]) -> Result<usize, Error> {
        let bytes = name.as_bytes();
        if bytes.len() >= output.len() {
            return Err(Error::BadLength);
        }
        output[..bytes.len()].copy_from_slice(bytes);
        output[bytes.len()] = 0;
        self.ioctl(IOCTL_GET_VARIABLE, false, output)
    }

    /// Sets one of the firmware's variables to a 32-bit value.
    pub fn set_variable(&mut self, name: &str, value: u32) -> Result<(), Error> {
        let mut buffer = [0; 64];
        let bytes = name.as_bytes();
        let end = bytes.len() + 1 + 4;
        let data = buffer.get_mut(..end).ok_or(Error::BadLength)?;
        data[..bytes.len()].copy_from_slice(bytes);
        data[bytes.len() + 1..].copy_from_slice(&value.to_le_bytes());
        self.ioctl(IOCTL_SET_VARIABLE, true, data)?;
        Ok(())
    }

    /// Sets one of the firmware's variables to some bytes.
    pub fn set_variable_bytes(&mut self, name: &str, value: &[u8]) -> Result<(), Error> {
        let mut buffer = [0; 256];
        let bytes = name.as_bytes();
        let end = bytes.len() + 1 + value.len();
        let data = buffer.get_mut(..end).ok_or(Error::BadLength)?;
        data[..bytes.len()].copy_from_slice(bytes);
        data[bytes.len() + 1..].copy_from_slice(value);
        self.ioctl(IOCTL_SET_VARIABLE, true, data)?;
        Ok(())
    }

    fn ioctl_set_int(&mut self, command: u32, value: u32) -> Result<(), Error> {
        let mut bytes = value.to_le_bytes();
        self.ioctl(command, true, &mut bytes)?;
        Ok(())
    }

    /// Whether the firmware can do the WPA handshake itself. It is what `brcmfmac/feature.c:289`
    /// asks: a firmware that cannot refuses the variable.
    pub fn has_own_supplicant(&mut self) -> Result<bool, Error> {
        let mut value = [0; 16];
        match self.get_variable("sup_wpa", &mut value) {
            Ok(_) => Ok(true),
            Err(Error::Firmware(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Readies the radio for scanning, as a station, without power saving, and asks for the
    /// results of scans and the steps of joining as events. Other events stay as the firmware has
    /// them. The order is that
    /// of `brcmfmac/cfg80211.c:7328-7342`, with `mpc` off as `:1157` turns it off for a scan.
    pub fn prepare_radio(&mut self) -> Result<(), Error> {
        self.set_variable("mpc", 0)?;
        self.ioctl_set_int(IOCTL_UP, 0)?;
        for (command, value) in IOCTL_SCAN_TIMES {
            self.ioctl_set_int(command, value)?;
        }
        // `brcm80211/include/defs.h:76` `PM_OFF`.
        self.ioctl_set_int(IOCTL_SET_PM, 0)?;
        // `brcmfmac/cfg80211.c:991,1011`: 1 is a station.
        self.ioctl_set_int(IOCTL_SET_INFRA, 1)?;

        let mut mask = [0; EVENT_MASK_BYTES];
        self.get_variable("event_msgs", &mut mask)?;
        for event in EVENTS {
            mask[(event / 8) as usize] |= 1 << (event % 8);
        }
        self.set_variable_bytes("event_msgs", &mask)
    }

    /// Skips the BCDC header of a frame on the event or data channel, and where it says its contents
    /// start: `brcmfmac/bcdc.c:302-321`.
    fn payload_start(&self, offset: usize, length: usize) -> Option<usize> {
        let header = self.frame[..length].get(offset..offset + BCDC_DATA_HEADER)?;
        if header[0] >> BCDC_DATA_VERSION_SHIFT != BCDC_DATA_VERSION {
            return None;
        }
        let start = offset + BCDC_DATA_HEADER + usize::from(header[3]) * 4;
        (start <= length).then_some(start)
    }

    /// Reads an event from a frame on the event channel, and where its data is.
    fn parse_event(&self, offset: usize, length: usize) -> Option<(Event, usize, usize)> {
        let event = self.payload_start(offset, length)?;
        let packet = &self.frame[event..length];
        if packet.len() < EVENT_MESSAGE + MESSAGE_SIZE
            || u16::from_be_bytes([packet[EVENT_PROTOCOL], packet[EVENT_PROTOCOL + 1]]) != ETHERTYPE_LINK_CONTROL
            || packet[EVENT_OUI..EVENT_OUI + 3] != BROADCOM_OUI
            || u16::from_be_bytes([packet[EVENT_SUBTYPE], packet[EVENT_SUBTYPE + 1]]) != SUBTYPE_EVENT
        {
            return None;
        }
        let message = &packet[EVENT_MESSAGE..];
        let be32 = |at: usize| u32::from_be_bytes([message[at], message[at + 1], message[at + 2], message[at + 3]]);
        let start = event + EVENT_MESSAGE + MESSAGE_SIZE;
        let end = start.saturating_add(be32(MESSAGE_LENGTH) as usize).min(length);
        let found = Event {
            code: be32(MESSAGE_CODE),
            status: be32(MESSAGE_STATUS),
            reason: be32(MESSAGE_REASON),
            flags: u16::from_be_bytes([message[MESSAGE_FLAGS], message[MESSAGE_FLAGS + 1]]),
        };
        Some((found, start, end))
    }

    /// Reads whatever is waiting: events kept from before first, then the next frame.
    pub fn receive(&mut self) -> Result<Incoming<'_>, Error> {
        if let Some(event) = self.queued_events[0].take() {
            self.queued_events.copy_within(1.., 0);
            self.queued_events[MAX_QUEUED_EVENTS - 1] = None;
            return Ok(Incoming::Event(event));
        }
        let Some((channel, offset, length)) = self.receive_frame()? else {
            return Ok(Incoming::Nothing);
        };
        if channel == CHANNEL_EVENT {
            return Ok(match self.parse_event(offset, length) {
                Some((event, _, _)) => Incoming::Event(event),
                None => Incoming::Nothing,
            });
        }
        if channel != CHANNEL_DATA {
            return Ok(Incoming::Nothing);
        }
        Ok(match self.payload_start(offset, length) {
            Some(start) => Incoming::Ethernet(&self.frame[start..length]),
            None => Incoming::Nothing,
        })
    }

    /// Sends an Ethernet frame, with its header. It leaves unencrypted unless the firmware holds a
    /// key for its destination.
    pub fn send_ethernet(&mut self, ethernet: &[u8]) -> Result<(), Error> {
        let total = HEADER_LENGTH + BCDC_DATA_HEADER + ethernet.len();
        let padded = total.next_multiple_of(4);
        if ethernet.len() < ETHERNET_HEADER || padded > FRAME_SIZE {
            return Err(Error::BadLength);
        }
        let begin = self.mct.count();
        self.wait_to_send(begin, CONTROL_TIMEOUT_MS * 1000 * COUNTS_PER_US)?;

        let sequence = self.tx_sequence;
        let frame = &mut self.frame;
        frame[..padded].fill(0);
        frame[0..2].copy_from_slice(&(total as u16).to_le_bytes());
        frame[2..4].copy_from_slice(&(!(total as u16)).to_le_bytes());
        let software = u32::from(sequence)
            | (u32::from(CHANNEL_DATA) << CHANNEL_SHIFT)
            | ((HEADER_LENGTH as u32) << DATA_OFFSET_SHIFT);
        frame[4..8].copy_from_slice(&software.to_le_bytes());
        // `brcmfmac/bcdc.c:267`: the rest of this header stays zero, as interface 0 and priority 0.
        frame[HEADER_LENGTH] = BCDC_DATA_VERSION << BCDC_DATA_VERSION_SHIFT;
        frame[HEADER_LENGTH + BCDC_DATA_HEADER..total].copy_from_slice(ethernet);

        let address = self.frame_address()?;
        let block = usize::from(FUNCTION2_BLOCK_SIZE);
        self.card.write(FUNCTION2, address, false, block, &self.frame[..padded])?;
        self.tx_sequence = sequence.wrapping_add(1);
        Ok(())
    }

    /// Asks the firmware to join a WPA2-Personal network at one access point, in the order of
    /// `brcmfmac/cfg80211.c` `brcmf_cfg80211_connect`. This only starts it: see
    /// [`connect`](Self::connect). Security is declared by [`wpa::OWN_RSN`].
    fn join(&mut self, ssid: &[u8], bssid: [u8; 6]) -> Result<(), Error> {
        if ssid.len() > 32 {
            return Err(Error::BadLength);
        }
        // Linux does not check this one: a firmware that refuses it writes its own element.
        match self.set_variable_bytes("wpaie", &wpa::OWN_RSN) {
            Ok(()) | Err(Error::Firmware(_)) => {}
            Err(error) => return Err(error),
        }
        self.set_variable("wpa_auth", WPA2_AUTH_PSK)?;
        self.set_variable("auth", AUTH_OPEN)?;
        self.set_variable("wsec", AES_ENABLED)?;

        let mut params = [0; JOIN_PARAMS_BYTES];
        params[..4].copy_from_slice(&(ssid.len() as u32).to_le_bytes());
        params[4..4 + ssid.len()].copy_from_slice(ssid);
        params[JOIN_SCAN_TYPE] = 0xFF;
        params[JOIN_SCAN_DEFAULTS].fill(0xFF);
        params[JOIN_BSSID..JOIN_BSSID + 6].copy_from_slice(&bssid);
        match self.set_variable_bytes("join", &params) {
            Err(Error::Firmware(_)) => {}
            other => return other,
        }

        // Older firmware lacks that variable: `brcmfmac/cfg80211.c`, the same function.
        let mut old = [0; OLD_JOIN_BYTES];
        old[..36].copy_from_slice(&params[..36]);
        old[OLD_JOIN_BSSID..OLD_JOIN_BSSID + 6].copy_from_slice(&bssid);
        self.ioctl(IOCTL_SET_SSID, true, &mut old)?;
        Ok(())
    }

    /// Hands a key to the chip, as `brcmfmac/cfg80211.c:2436` does: with `peer` for traffic with
    /// one station, and without for broadcasts, which are the primary key.
    fn install_key(
        &mut self,
        index: u8,
        key: &[u8; 16],
        peer: Option<[u8; 6]>,
        rsc: [u8; 6],
    ) -> Result<(), Error> {
        let mut data = [0; KEY_BYTES];
        data[KEY_INDEX..KEY_INDEX + 4].copy_from_slice(&u32::from(index).to_le_bytes());
        data[KEY_LENGTH..KEY_LENGTH + 4].copy_from_slice(&(key.len() as u32).to_le_bytes());
        data[KEY_DATA..KEY_DATA + key.len()].copy_from_slice(key);
        data[KEY_ALGORITHM..KEY_ALGORITHM + 4].copy_from_slice(&KEY_ALGORITHM_AES.to_le_bytes());
        data[KEY_IV_INITIALIZED..KEY_IV_INITIALIZED + 4].copy_from_slice(&1u32.to_le_bytes());
        data[KEY_RX_IV_HIGH..KEY_RX_IV_HIGH + 4].copy_from_slice(&rsc[2..6]);
        data[KEY_RX_IV_LOW..KEY_RX_IV_LOW + 2].copy_from_slice(&rsc[..2]);
        match peer {
            Some(peer) => data[KEY_PEER..KEY_PEER + 6].copy_from_slice(&peer),
            None => data[KEY_FLAGS..KEY_FLAGS + 4].copy_from_slice(&KEY_FLAG_PRIMARY.to_le_bytes()),
        }
        self.set_variable_bytes("wsec_key", &data)
    }

    /// Joins a network and runs its handshake. Returns once traffic and broadcast keys are in
    /// place, and tells of each step. [`prepare_radio`](Self::prepare_radio) goes first.
    pub fn connect(
        &mut self,
        ssid: &[u8],
        supplicant: &mut Supplicant,
        timeout_ms: u64,
        mut report: impl FnMut(Activity),
    ) -> Result<(), Error> {
        self.associated = false;
        self.join(ssid, supplicant.access_point())?;
        let begin = self.mct.count();
        while self.mct.count() - begin < timeout_ms * 1000 * COUNTS_PER_US {
            // Nothing but the handshake is worth keeping until the keys are in.
            self.serve(supplicant, &mut [], &mut report)?;
            if self.associated && supplicant.is_complete() {
                return Ok(());
            }
        }
        Err(Error::JoinTimeout)
    }

    /// Serves a connection once: takes one thing that is waiting, answers what the network asks of
    /// it, and tells of what happened. If that was a network frame other than the handshake's, it
    /// goes in `buffer`, and this returns its length; one too long for `buffer` is dropped. Returns
    /// an error if the connection ends.
    pub fn poll(
        &mut self,
        supplicant: &mut Supplicant,
        buffer: &mut [u8],
        mut report: impl FnMut(Activity),
    ) -> Result<Option<usize>, Error> {
        self.serve(supplicant, buffer, &mut report)
    }

    /// Takes one thing that is waiting, if any.
    fn serve(
        &mut self,
        supplicant: &mut Supplicant,
        buffer: &mut [u8],
        report: &mut impl FnMut(Activity),
    ) -> Result<Option<usize>, Error> {
        let mut eapol = [0; EAPOL_MAX];
        let mut eapol_length = None;
        let mut received = None;
        match self.receive()? {
            Incoming::Nothing => {}
            Incoming::Event(event) => {
                report(Activity::Event(event));
                self.follow_event(event)?;
            }
            Incoming::Ethernet(frame) if frame.len() >= ETHERNET_HEADER => {
                let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
                if ethertype != ETHERTYPE_EAPOL {
                    if let Some(space) = buffer.get_mut(..frame.len()) {
                        space.copy_from_slice(frame);
                        received = Some(frame.len());
                    }
                } else if frame[6..12] == supplicant.access_point() {
                    let body = &frame[ETHERNET_HEADER..];
                    if let Some(space) = eapol.get_mut(..body.len()) {
                        space.copy_from_slice(body);
                        eapol_length = Some(body.len());
                    }
                }
            }
            Incoming::Ethernet(_) => {}
        }
        if let Some(length) = eapol_length {
            self.answer_handshake(supplicant, &eapol[..length], report)?;
        }
        Ok(received)
    }

    fn follow_event(&mut self, event: Event) -> Result<(), Error> {
        let link_up = event.flags & EVENT_FLAG_LINK != 0;
        match event.code {
            EVENT_SET_SSID if event.status != STATUS_SUCCESS => {
                return Err(Error::JoinFailed {
                    status: event.status,
                    reason: event.reason,
                });
            }
            EVENT_SET_SSID => self.associated = true,
            EVENT_LINK if link_up => self.associated = true,
            // Only an established connection can be lost. Before that the firmware reports failure
            // through its answer to joining, and these may just be it clearing the way.
            EVENT_LINK | EVENT_DEAUTH | EVENT_DEAUTH_IND | EVENT_DISASSOC | EVENT_DISASSOC_IND
                if self.associated =>
            {
                return Err(Error::Disconnected {
                    event: event.code,
                    reason: event.reason,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn answer_handshake(
        &mut self,
        supplicant: &mut Supplicant,
        eapol: &[u8],
        report: &mut impl FnMut(Activity),
    ) -> Result<(), Error> {
        let mut reply = [0; ETHERNET_HEADER + wpa::REPLY_MAX];
        let step = match supplicant.handle(eapol, &mut reply[ETHERNET_HEADER..]) {
            Ok(step) => step,
            Err(error) if error.is_fatal() => return Err(Error::Wpa(error)),
            Err(error) => {
                report(Activity::Ignored(error));
                return Ok(());
            }
        };
        report(Activity::Handshake(step.message));

        let access_point = supplicant.access_point();
        reply[..6].copy_from_slice(&access_point);
        reply[6..12].copy_from_slice(&supplicant.own());
        reply[12..14].copy_from_slice(&ETHERTYPE_EAPOL.to_be_bytes());
        self.send_ethernet(&reply[..ETHERNET_HEADER + step.reply_length])?;

        if step.pairwise.is_some() || step.group.is_some() {
            delay_ms(self.mct, MESSAGE_4_SETTLE_MS);
        }
        if let Some(key) = step.pairwise {
            self.install_key(0, &key, Some(access_point), [0; 6])?;
        }
        if let Some(group) = step.group {
            self.install_key(group.index, &group.key, None, group.rsc)?;
        }
        Ok(())
    }

    /// Scans every channel the firmware allows, actively, and calls `found` for what each
    /// result frame holds, with the network's security element if it has one. The same network
    /// usually comes more than once. Returns the status the
    /// firmware ended on, 0 if the scan finished normally. Call [`prepare_radio`](Self::prepare_radio)
    /// first.
    pub fn scan(&mut self, timeout_ms: u64, mut found: impl FnMut(&Bss, &[u8])) -> Result<u32, Error> {
        let mut params = [0; SCAN_PARAMS_BYTES];
        params[0..4].copy_from_slice(&SCAN_REQUEST_VERSION.to_le_bytes());
        params[4..6].copy_from_slice(&SCAN_ACTION_START.to_le_bytes());
        params[6..8].copy_from_slice(&SCAN_SYNC_ID.to_le_bytes());
        params[SCAN_BSSID..SCAN_BSSID + 6].fill(0xFF);
        params[SCAN_BSS_TYPE] = BSS_TYPE_ANY;
        params[SCAN_DEFAULTS].fill(0xFF);
        self.set_variable_bytes("escan", &params)?;

        let begin = self.mct.count();
        while self.mct.count() - begin < timeout_ms * 1000 * COUNTS_PER_US {
            let Some((channel, offset, length)) = self.receive_frame()? else {
                continue;
            };
            if channel != CHANNEL_EVENT {
                continue;
            }
            let Some((event, start, end)) = self.parse_event(offset, length) else {
                continue;
            };
            if event.code != EVENT_SCAN_RESULT {
                self.keep_event(channel, offset, length);
                continue;
            }
            if event.status != STATUS_PARTIAL {
                return Ok(event.status);
            }
            let data = &self.frame[start..end];
            if let Some(entries) = data.get(RESULT_HEADER..) {
                let declared = entries
                    .get(BSS_LENGTH..BSS_LENGTH + 4)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
                if let Some((bss, rsn)) = declared.and_then(|length| entries.get(..length)).and_then(parse_bss) {
                    found(&bss, rsn);
                }
            }
        }
        Err(Error::ScanTimeout)
    }

    /// Tells the firmware not to merge frames, which this driver does not take apart. The name is
    /// from `brcmfmac/sdio.c:3554`.
    pub fn disable_glomming(&mut self) -> Result<(), Error> {
        self.set_variable("bus:txglom", 0)
    }
}
