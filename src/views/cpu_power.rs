use crate::Message;
use crate::style::*;
use crate::views::ViewSnapshot;
use iced::widget::{button, column, container, row, space, text, text_input};
use iced::{Element, Length};

/// Turbo readout text: current/limit, whichever was readable. `None` hides
/// the row. Pure so the four branches stay pinned by test.
pub(crate) fn turbo_text(now_ghz: Option<f64>, limit_ghz: Option<f64>) -> Option<String> {
    match (now_ghz, limit_ghz) {
        (None, None) => None,
        (Some(n), Some(l)) => Some(format!("{n:.1} / {l:.1} GHz")),
        (Some(n), None) => Some(format!("{n:.1} GHz")),
        (None, Some(l)) => Some(format!("max {l:.1} GHz")),
    }
}

/// Read-only AMD CPU Power card. Writes land in a later phase; this card
/// exists so AMD users can verify the RyzenSMU session and PM-table values.
fn amd_power_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    let pal = snap.palette;
    let info = &snap.amd_power;
    let settings_label = if snap.show_cpu_power_settings {
        "[-] Settings"
    } else {
        "[+] Settings"
    };
    let settings_button = button(text(settings_label).size(FONT_SMALL)).style(btn_style);
    let settings_button = if info.available {
        settings_button.on_press(Message::ToggleCpuPowerSettings)
    } else {
        settings_button
    };
    let mut content = column![
        row![
            text("CPU Power")
                .size(FONT_SECTION)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.header)
                }),
            space::horizontal(),
            settings_button,
        ],
        text(if snap.amd_preview_full {
            "AMD RyzenSMU — preview (writes disabled)"
        } else if snap.amd_preview {
            "AMD RyzenSMU — read-only preview"
        } else {
            "AMD RyzenSMU — experimental"
        })
        .size(FONT_SMALL)
        .style(move |_theme| iced::widget::text::Style {
            color: Some(pal.gray),
        }),
    ]
    .spacing(4);

    if !info.available {
        if let Some(ref err) = info.error {
            content = content.push(text(err.to_string()).size(FONT_SMALL).style(move |_theme| {
                iced::widget::text::Style {
                    color: Some(pal.error),
                }
            }));
        }
        if !crate::cpu_power::is_pawnio_installed() {
            content = content.push(
                button(text("Install PawnIO").size(FONT_BODY))
                    .on_press(Message::InstallPawnIO)
                    .style(btn_style),
            );
        } else if !crate::cpu_power::modules_downloaded_amd() {
            content = content.push(
                text("PawnIO Modules required (RyzenSMU.bin)")
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            );
            let download_button =
                button(text("Download PawnIO Modules").size(FONT_BODY)).style(btn_style);
            content = content.push(if snap.pawnio_busy {
                download_button
            } else {
                download_button.on_press(Message::DownloadPawnIOModules)
            });
            content = content.push(
                row![
                    button(text("Open Modules Folder").size(FONT_BODY))
                        .on_press(Message::OpenModulesDir)
                        .style(btn_style),
                    button(text("Redetect Modules").size(FONT_BODY))
                        .on_press(Message::RedetectModules)
                        .style(btn_style),
                ]
                .spacing(8),
            );
        }
        if let Some(ref status) = snap.pawnio_status {
            content = content.push(text(status.as_str()).size(FONT_SMALL).style(move |_theme| {
                iced::widget::text::Style {
                    color: Some(pal.gray),
                }
            }));
        }
        return content.into();
    }

    content = content.push(
        text(format!(
            "{}  ·  SMU {}.{}.{}  ·  PM table 0x{:06X}",
            info.code_name_name(),
            (info.smu_version >> 24) & 0xFF,
            (info.smu_version >> 16) & 0xFF,
            (info.smu_version >> 8) & 0xFF,
            info.pm_table_version
        ))
        .size(FONT_SMALL)
        .style(move |_theme| iced::widget::text::Style {
            color: Some(pal.gray),
        }),
    );
    for (label, value) in [
        ("STAPM", format!("{:.1} W", info.stapm_w)),
        ("PPT Fast", format!("{:.1} W", info.fast_w)),
        ("PPT Slow", format!("{:.1} W", info.slow_w)),
        ("Tctl", format!("{:.1} °C", info.tctl_c)),
    ] {
        content = content.push(
            row![
                text(label).size(FONT_BODY),
                space::horizontal(),
                text(value).size(FONT_BODY),
            ]
            .spacing(8),
        );
    }
    if let Some(apu_slow) = info.apu_slow_w {
        content = content.push(
            row![
                text("APU Slow").size(FONT_BODY),
                space::horizontal(),
                text(format!("{apu_slow:.1} W")).size(FONT_BODY),
            ]
            .spacing(8),
        );
    }
    let mut settings = column![].spacing(4).padding(4);
    if snap.show_cpu_power_settings {
        settings = settings.push(
            row![
                text("Experimental").size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.warning),
                    }
                }),
                text("unverified AMD writes — verify the PM-table readback")
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        );
        let (saved, has_default) = if snap.amd_profile_is_ac {
            (snap.amd_profile_ac_saved, snap.amd_profile_ac_initial)
        } else {
            (
                snap.amd_profile_battery_saved,
                snap.amd_profile_battery_initial,
            )
        };
        let saved_note = if saved {
            "saved"
        } else if has_default {
            "PM-table baseline"
        } else {
            "live"
        };
        let is_ac = snap.amd_profile_is_ac;
        settings = settings.push(
            row![
                text("Profile:").size(FONT_BODY),
                button(text("AC").size(FONT_BODY))
                    .on_press(Message::AmdProfileSourceChanged(true))
                    .style(move |theme, status| mode_style(theme, status, is_ac)),
                button(text("Battery").size(FONT_BODY))
                    .on_press(Message::AmdProfileSourceChanged(false))
                    .style(move |theme, status| mode_style(theme, status, !is_ac)),
                text(saved_note)
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );
        if !snap.amd_preview || snap.amd_preview_full {
            for (label, edit, on_input) in [
                (
                    "STAPM:",
                    &snap.amd_stapm_edit,
                    Message::AmdStapmChanged as fn(String) -> Message,
                ),
                ("PPT Fast:", &snap.amd_fast_edit, Message::AmdFastChanged),
                ("PPT Slow:", &snap.amd_slow_edit, Message::AmdSlowChanged),
                ("Tctl:", &snap.amd_tctl_edit, Message::AmdTctlChanged),
            ] {
                settings = settings.push(
                    row![
                        text(label).size(FONT_BODY),
                        text_input("", edit)
                            .width(Length::Fixed(70.0))
                            .on_input(on_input)
                            .style(input_style),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                );
            }
            settings = settings.push(
                row![
                    iced::widget::checkbox(snap.amd_write_enabled)
                        .on_toggle(Message::AmdWriteEnabledToggled)
                        .style(checkbox_style),
                    text("Enable experimental AMD writes").size(FONT_SMALL),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            );
            let apply = button(text("Apply").size(FONT_BODY)).style(btn_style);
            let reset = button(text("Reset").size(FONT_BODY)).style(btn_style);
            let can_write = !snap.amd_apply_busy && snap.amd_write_enabled;
            settings = settings.push(if can_write {
                row![
                    apply.on_press(Message::AmdApply),
                    reset.on_press(Message::AmdReset)
                ]
                .spacing(8)
            } else {
                row![apply, reset].spacing(8)
            });
        }
        content = content.push(container(settings).width(Length::Fill).padding(8).style(
            move |_theme| iced::widget::container::Style {
                background: Some(pal.settings_bg.into()),
                border: iced::Border::default().rounded(4).color(pal.dark).width(1),
                ..Default::default()
            },
        ));
    }
    content = content.push(
        row![
            button(text("Copy diagnostics").size(FONT_SMALL))
                .on_press(Message::AmdCopyDiagnostics)
                .style(btn_style),
        ]
        .spacing(6),
    );
    if let Some(ref err) = snap.cpu_power_error {
        content = content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.error),
            }
        }));
    }
    if snap.amd_preview {
        content = content.push(
            text("PREVIEW: mock PM-table data; writes disabled")
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.warning),
                }),
        );
    } else {
        content = content.push(
            text("AMD writes are experimental. Verify the PM-table readback after Apply.")
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                }),
        );
    }
    content.into()
}

pub(crate) fn cpu_power_section(snap: &ViewSnapshot) -> Element<'_, Message> {
    if snap.amd_cpu {
        return amd_power_section(snap);
    }
    let mut content = column![].spacing(2);
    let pal = snap.palette;

    let settings_label = if snap.show_cpu_power_settings {
        "[-] Settings"
    } else {
        "[+] Settings"
    };
    content = content.push(row![
        text("CPU Power")
            .size(FONT_SECTION)
            .style(move |_theme| iced::widget::text::Style {
                color: Some(pal.header)
            }),
        space::horizontal(),
        if snap.intel_cpu {
            button(text(settings_label).size(FONT_SMALL))
                .on_press(Message::ToggleCpuPowerSettings)
                .style(btn_style)
        } else {
            button(text(settings_label).size(FONT_SMALL)).style(btn_style)
        },
    ]);

    if !snap.intel_cpu {
        return crate::views::not_supported_section(pal, "CPU Power");
    }

    let info = &snap.cpu_power;

    if !info.available {
        let msg = info
            .unavailable
            .map(crate::cpu_power::CpuPowerUnavailable::message)
            .unwrap_or("PawnIO driver not available");
        content = content.push(text(msg).size(FONT_BODY).style(move |_theme| {
            iced::widget::text::Style {
                color: Some(pal.gray),
            }
        }));
        if !crate::cpu_power::is_pawnio_installed() {
            let install_button = button(text("Install PawnIO").size(FONT_BODY)).style(btn_style);
            content = content.push(if snap.pawnio_busy {
                install_button
            } else {
                install_button.on_press(Message::InstallPawnIO)
            });
            if let Some(ref status) = snap.pawnio_status {
                content =
                    content.push(text(status.as_str()).size(FONT_SMALL).style(move |_theme| {
                        iced::widget::text::Style {
                            color: Some(pal.gray),
                        }
                    }));
            }
            if let Some(ref err) = snap.modules_download_error {
                content = content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.error),
                    }
                }));
            }
        } else if !crate::cpu_power::modules_downloaded() {
            content = content.push(text("PawnIO Modules required").size(FONT_SMALL).style(
                move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                },
            ));
            if let Some(ref status) = snap.pawnio_status {
                content =
                    content.push(text(status.as_str()).size(FONT_SMALL).style(move |_theme| {
                        iced::widget::text::Style {
                            color: Some(pal.gray),
                        }
                    }));
            }
            if let Some(ref err) = snap.modules_download_error {
                content = content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.error),
                    }
                }));
            }
            let download_button =
                button(text("Download PawnIO Modules").size(FONT_BODY)).style(btn_style);
            content = content.push(if snap.pawnio_busy {
                download_button
            } else {
                download_button.on_press(Message::DownloadPawnIOModules)
            });
            content = content.push(
                text("Manual: download latest release_*.zip from https://github.com/namazso/PawnIO.Modules/releases/latest and place IntelMSR.bin / IntelMCHBAR.bin into the modules folder (Open Modules Folder)")
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            );
            content = content.push(
                row![
                    button(text("Open Modules Folder").size(FONT_BODY))
                        .on_press(Message::OpenModulesDir)
                        .style(btn_style),
                    button(text("Redetect Modules").size(FONT_BODY))
                        .on_press(Message::RedetectModules)
                        .style(btn_style),
                ]
                .spacing(8),
            );
        } else {
            // PawnIO and modules present but driver read still fails (needs reboot/elevation)
            if let Some(ref status) = snap.pawnio_status {
                content =
                    content.push(text(status.as_str()).size(FONT_SMALL).style(move |_theme| {
                        iced::widget::text::Style {
                            color: Some(pal.gray),
                        }
                    }));
            }
            if let Some(ref err) = snap.cpu_power_error {
                content = content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.error),
                    }
                }));
            }
            content = content.push(
                text("PawnIO driver may require reboot or elevation. Try Retry, or reinstall PawnIO and reboot.")
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            );
            content = content.push(
                row![
                    button(text("Retry").size(FONT_BODY))
                        .on_press(Message::RefreshCpuPower)
                        .style(btn_style),
                    button(text("Redetect Modules").size(FONT_BODY))
                        .on_press(Message::RedetectModules)
                        .style(btn_style),
                    button(text("Open Modules Folder").size(FONT_BODY))
                        .on_press(Message::OpenModulesDir)
                        .style(btn_style),
                ]
                .spacing(8),
            );
        }
        return content.into();
    }

    // Show effective (min) limit; CPU enforces lower of MSR and MMIO.
    let pl1_color = if snap.pl_custom_applied {
        pal.green
    } else {
        pal.header
    };
    let pl2_color = if snap.pl_custom_applied {
        pal.green
    } else {
        pal.header
    };
    let mut limit_row = row![
        text("  PL1:".to_string()).size(FONT_BODY),
        text(format!("{:.1}W", info.effective_pl1()))
            .size(FONT_BODY)
            .style(move |_theme| iced::widget::text::Style {
                color: Some(pl1_color)
            }),
        text("  PL2:".to_string()).size(FONT_BODY),
        text(format!("{:.1}W", info.effective_pl2()))
            .size(FONT_BODY)
            .style(move |_theme| iced::widget::text::Style {
                color: Some(pl2_color)
            }),
    ]
    .spacing(4);
    if let Some(package_watts) = info.package_watts {
        limit_row = limit_row
            .push(text("  Package:".to_string()).size(FONT_BODY))
            .push(
                text(format!("{:.1}W", package_watts))
                    .size(FONT_BODY)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header),
                    }),
            );
    }
    content = content.push(limit_row);
    // Turbo telemetry is display-only: current/limit, whichever was readable.
    if let Some(t) = turbo_text(info.turbo_now_ghz, info.turbo_limit_ghz) {
        content = content.push(
            row![
                text("  Turbo:".to_string()).size(FONT_BODY),
                text(t).size(FONT_BODY).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.header),
                    }
                }),
            ]
            .spacing(4),
        );
    }
    content = content.push(
        text("MSR-controlled (MMIO mirror is read-only)")
            .size(FONT_SMALL)
            .style(move |_theme| iced::widget::text::Style {
                color: Some(pal.gray),
            }),
    );

    if snap.show_cpu_power_settings {
        let mut settings_content = column![].spacing(4).padding(4);

        settings_content =
            settings_content.push(text("MSR (writable via MSR 0x610)").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL1: {:.1}W ({:.2}s)",
                    info.pl1_msr, info.pl1_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl1_msr_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(if info.pl1_msr_enabled {
                        pal.green
                    } else {
                        pal.gray
                    })
                }),
                text(if info.pl1_msr_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
                    }),
            ]
            .spacing(4),
        );
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL2: {:.1}W ({:.2}s)",
                    info.pl2_msr, info.pl2_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl2_msr_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(if info.pl2_msr_enabled {
                        pal.green
                    } else {
                        pal.gray
                    })
                }),
                text(if info.pl2_msr_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
                    }),
            ]
            .spacing(4),
        );

        settings_content = settings_content.push(text("MMIO (Read-only)").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL1: {:.1}W ({:.2}s)",
                    info.pl1_mmio, info.pl1_mmio_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl1_mmio_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(if info.pl1_mmio_enabled {
                        pal.green
                    } else {
                        pal.gray
                    })
                }),
                text(if info.pl1_mmio_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
                    }),
            ]
            .spacing(4),
        );
        settings_content = settings_content.push(
            row![
                text(format!(
                    "  PL2: {:.1}W ({:.2}s)",
                    info.pl2_mmio, info.pl2_mmio_time_s
                ))
                .size(FONT_BODY),
                text(if info.pl2_mmio_enabled {
                    " [En]"
                } else {
                    " [Dis]"
                })
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(if info.pl2_mmio_enabled {
                        pal.green
                    } else {
                        pal.gray
                    })
                }),
                text(if info.pl2_mmio_clamped { " [Cl]" } else { "" })
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.header)
                    }),
            ]
            .spacing(4),
        );
        settings_content = settings_content.push(
            text("Official IntelMCHBAR is read-only: writes go to MSR 0x610 only. The MMIO mirror is displayed, not written.")
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                }),
        );
        if !info.mmio_available {
            settings_content = settings_content.push(
                text("MMIO read unavailable; MMIO values may show zero.")
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            );
        }

        // Source selector: Apply saves the edited values into the selected
        // AC/Battery profile. While the About keep-alive toggle is on, the
        // matching profile is auto-applied on every source change.
        let (saved, has_default) = if snap.pl_profile_is_ac {
            (snap.pl_profile_ac_saved, snap.pl_profile_ac_initial)
        } else {
            (
                snap.pl_profile_battery_saved,
                snap.pl_profile_battery_initial,
            )
        };
        let saved_note = if saved {
            "saved"
        } else if has_default {
            "MMIO default"
        } else {
            "not saved"
        };
        let is_ac = snap.pl_profile_is_ac;
        settings_content = settings_content.push(
            row![
                text("  Profile:").size(FONT_BODY),
                button(text("AC").size(FONT_BODY))
                    .on_press(Message::CpuPowerProfileSourceChanged(true))
                    .style(move |theme, status| mode_style(theme, status, is_ac)),
                button(text("Battery").size(FONT_BODY))
                    .on_press(Message::CpuPowerProfileSourceChanged(false))
                    .style(move |theme, status| mode_style(theme, status, !is_ac)),
                text(saved_note)
                    .size(FONT_SMALL)
                    .style(move |_theme| iced::widget::text::Style {
                        color: Some(pal.gray),
                    }),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );

        // Editable PL1/PL2 writes to MSR 0x610. Only the PL1 time window is
        // editable; the PL2 window follows the live register value instead of
        // taking a second field, so an apply can never pair a PL2 wattage with
        // a stale or zero tau.
        settings_content = settings_content.push(text("PL1/PL2 Control").size(FONT_BODY));
        settings_content = settings_content.push(
            row![
                text("  PL1:").size(FONT_BODY),
                text_input("W", &snap.pl1_edit)
                    .width(Length::Fixed(60.0))
                    .on_input(Message::CpuPowerPl1Changed)
                    .style(input_style),
                iced::widget::checkbox(snap.pl1_enabled)
                    .on_toggle(Message::CpuPowerPl1EnabledToggled)
                    .style(checkbox_style),
                text("En").size(FONT_SMALL),
                iced::widget::checkbox(snap.pl1_clamped)
                    .on_toggle(Message::CpuPowerPl1ClampedToggled)
                    .style(checkbox_style),
                text("Cl").size(FONT_SMALL),
                text("T:").size(FONT_SMALL),
                text_input("s", &snap.pl1_time_edit)
                    .width(Length::Fixed(50.0))
                    .on_input(Message::CpuPowerPl1TimeChanged)
                    .style(input_style),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );
        settings_content = settings_content.push(
            row![
                text("  PL2:").size(FONT_BODY),
                text_input("W", &snap.pl2_edit)
                    .width(Length::Fixed(60.0))
                    .on_input(Message::CpuPowerPl2Changed)
                    .style(input_style),
                iced::widget::checkbox(snap.pl2_enabled)
                    .on_toggle(Message::CpuPowerPl2EnabledToggled)
                    .style(checkbox_style),
                text("En").size(FONT_SMALL),
                iced::widget::checkbox(snap.pl2_clamped)
                    .on_toggle(Message::CpuPowerPl2ClampedToggled)
                    .style(checkbox_style),
                text("Cl").size(FONT_SMALL),
            ]
            .spacing(4)
            .align_y(iced::Alignment::Center),
        );

        if let Some(ref status) = snap.cpu_power_status {
            settings_content = settings_content.push(text(status.as_str()).size(FONT_SMALL).style(
                move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                },
            ));
        }
        if let Some(ref err) = snap.cpu_power_error {
            settings_content =
                settings_content.push(text(err.as_str()).size(FONT_SMALL).style(move |_theme| {
                    iced::widget::text::Style {
                        color: Some(pal.error),
                    }
                }));
        }

        settings_content = settings_content.push(
            row![
                button(text("Apply").size(FONT_BODY))
                    .on_press(Message::CpuPowerApply)
                    .style(btn_style),
                button(text("Reset").size(FONT_BODY))
                    .on_press(Message::CpuPowerSyncReset)
                    .style(btn_style),
            ]
            .spacing(8),
        );
        settings_content = settings_content.push(
            text("Apply saves the selected profile. Profiles switch automatically only while \"Keep power limits applied\" is on (About).")
                .size(FONT_SMALL)
                .style(move |_theme| iced::widget::text::Style {
                    color: Some(pal.gray),
                }),
        );

        content = content.push(
            container(settings_content)
                .width(Length::Fill)
                .padding(8)
                .style(move |_theme| iced::widget::container::Style {
                    background: Some(pal.settings_bg.into()),
                    border: iced::Border::default().rounded(4).color(pal.dark).width(1),
                    ..Default::default()
                }),
        );
    }

    content.into()
}
