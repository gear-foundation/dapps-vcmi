use std::sync::{
    atomic::{AtomicBool, Ordering::Relaxed},
    Arc,
};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};

use gclient::WSAddress;
use gear_connector_api::{VcmiCommand, VcmiReply, VcmiSavedGame};
use gstd::ActorId;
use tauri::{PhysicalSize, Size, Window, LogicalSize};
use tauri_plugin_positioner::{Position, WindowExt};

use crate::{
    gear_client::{GearCommand, GearReply, RECV_TIMEOUT},
    ipfs_client::{IpfsCommand, IpfsReply},
    lobby::{LobbyClient, LobbyCommand, LobbyReply},
    program_io::{Action, ArchiveDescription, Event, GameState},
    GuiCommand,
};

pub enum Recipient {
    GearClient,
    Vcmi,
    Gui,
}

pub enum Message {
    VcmiCommand,
    VcmiReply,
    Action,
    Event,
}

pub struct Logic {
    need_stop: Arc<AtomicBool>,
    gear_command_sender: Sender<GearCommand>,
    gear_reply_receiver: Receiver<GearReply>,
    vcmi_command_receiver: Receiver<VcmiCommand>,
    vcmi_reply_sender: Sender<VcmiReply>,
    ipfs_reply_receiver: Receiver<IpfsReply>,
    ipfs_command_sender: Sender<IpfsCommand>,
    gui_command_receiver: Receiver<GuiCommand>,
    lobby_command_sender: Sender<LobbyCommand>,
    lobby_reply_receiver: Receiver<LobbyReply>,
    main_window: Window,
    log_window: Window,
}

impl Logic {
    pub fn new(
        need_stop: Arc<AtomicBool>,
        gear_command_sender: Sender<GearCommand>,
        gear_reply_receiver: Receiver<GearReply>,
        vcmi_command_receiver: Receiver<VcmiCommand>,
        vcmi_reply_sender: Sender<VcmiReply>,
        ipfs_reply_receiver: Receiver<IpfsReply>,
        ipfs_command_sender: Sender<IpfsCommand>,
        gui_command_receiver: Receiver<GuiCommand>,
        lobby_command_sender: Sender<LobbyCommand>,
        lobby_reply_receiver: Receiver<LobbyReply>,

        main_window: Window,
        log_window: Window,
    ) -> Self {
        Self {
            need_stop,
            gear_command_sender,
            gear_reply_receiver,
            vcmi_command_receiver,
            vcmi_reply_sender,
            ipfs_reply_receiver,
            ipfs_command_sender,
            gui_command_receiver,
            lobby_command_sender,
            lobby_reply_receiver,
            main_window,
            log_window,
        }
    }

    pub async fn run(&mut self) {
        while !self.need_stop.load(Relaxed) {
            self.process_gui_command().await;
            self.process_vcmi_command().await;
        }
    }

    fn connect_to_gear(&self) {
        self.main_window.center().unwrap();
        self.main_window.show().unwrap();
        self.main_window.set_focus().unwrap();
        self.vcmi_reply_sender
            .send(VcmiReply::ConnectDialogShowed)
            .expect("Error in another thread");
    }

    fn show_load_game_dialog(&self) {
        let command = GearCommand::GetSavedGames;
        self.gear_command_sender.send(command).expect("Can't send");
        let reply = self.gear_reply_receiver.recv().expect("Can't recv");

        match reply {
            GearReply::SavedGames(games) => {}
            _ => unreachable!("Unexpected reply to GetSavedGames command"),
        }
        self.vcmi_reply_sender
            .send(VcmiReply::LoadGameDialogShowed)
            .expect("Error in another thread");
    }

    fn save(&self, filename: String, compressed_archive: Vec<u8>) {
        let archive_name = format!("{filename}");

        tracing::info!("Archive len: {}", compressed_archive.len());

        let command = IpfsCommand::UploadData {
            filename,
            data: compressed_archive,
        };
        self.ipfs_command_sender.send(command).expect("Send error");

        let reply = self.ipfs_reply_receiver.recv().expect("Recv error");

        if let IpfsReply::Uploaded { name, hash } = reply {
            let saver_id = ActorId::default();
            let tar = ArchiveDescription {
                filename: archive_name,
                name,
                hash,
            };

            let gear_command = GearCommand::SendAction(Action::Save(GameState {
                saver_id,
                archive: tar,
            }));
            self.gear_command_sender
                .send(gear_command)
                .expect("Send error");
            let gear_reply = self.gear_reply_receiver.recv().expect("Recv error");

            if let GearReply::Event(e) = gear_reply {
                if matches!(e, Event::Saved) {
                    self.vcmi_reply_sender
                        .send(VcmiReply::Saved)
                        .expect("Send error");
                    return;
                }
            }
        }

        unreachable!();
    }

    fn load_all(&self) {
        self.gear_command_sender
            .send(GearCommand::GetSavedGames)
            .expect("Send error");

        let gear_reply = self.gear_reply_receiver.recv().expect("Recv Error");
        match gear_reply {
            GearReply::SavedGames(games) => {
                let mut archives = Vec::with_capacity(games.len());
                for (_actor_id, state) in games.into_iter() {
                    let hash = state.archive.hash;
                    let ipfs_command = IpfsCommand::DownloadData { hash };
                    self.ipfs_command_sender
                        .send(ipfs_command)
                        .expect("Send err");
                    let ipfs_reply = self.ipfs_reply_receiver.recv().expect("Recv err");
                    match ipfs_reply {
                        IpfsReply::Downloaded { data } => {
                            archives.push(VcmiSavedGame {
                                filename: state.archive.filename,
                                data,
                            });
                        }
                        _ => unreachable!("Wrong reply to Ipfs Download command"),
                    }
                }
                let vcmi_reply = VcmiReply::AllLoaded { archives };
                self.vcmi_reply_sender.send(vcmi_reply).expect("Send err");
            }
            _ => unreachable!("Wrong reply to GetSavedGames"),
        }
    }

    async fn update_balance(&self) {
        self.gear_command_sender
            .send(GearCommand::GetFreeBalance)
            .expect("Send Error");

        let reply = self.gear_reply_receiver.recv().expect("Recv error");
        match reply {
            GearReply::FreeBalance(balance) => {
                self.log_window.emit("update_balance", balance).unwrap();
                tracing::info!("Free balance: {}", balance);
            }
            _ => unreachable!("Reply {reply:?} is wrong to command FreeBalance"),
        }
    }

    async fn process_vcmi_command(&self) {
        match self.vcmi_command_receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(vcmi_command) => match vcmi_command {
                VcmiCommand::Connect => self.connect_to_gear(),
                VcmiCommand::Save {
                    filename,
                    compressed_archive,
                } => {
                    self.save(filename, compressed_archive);
                    self.update_balance().await;
                }
                VcmiCommand::Load(name) => self
                    .gear_command_sender
                    .send(GearCommand::SendAction(Action::Load { hash: name }))
                    .expect("Error in another thread"),
                VcmiCommand::ShowLoadGameDialog => self.show_load_game_dialog(),
                VcmiCommand::LoadAll => self.load_all(),
            },
            Err(e) if e == RecvTimeoutError::Timeout => {}
            Err(e) => {
                tracing::error!("Error in another thread: {}", e);
                self.need_stop.store(true, Relaxed);
            }
        }
    }

    fn connect(&self, address: String, program_id: String, account_id: String, password: String) {
        // let address = WSAddress::new("ws://localhost", 9944);
        let address = WSAddress::new(address, 443);
        self.gear_command_sender
            .send(GearCommand::ConnectToNode {
                address,
                program_id,
                password,
                account_id,
            })
            .expect("Error in another thread");

        let reply = self.gear_reply_receiver.recv().expect("Recv error");

        match reply {
            GearReply::Connected => {
                self.main_window.center().unwrap();
                self.main_window.hide().unwrap();
                self.log_window.show().unwrap();

                self.log_window.move_window(Position::TopRight).unwrap();
                self.vcmi_reply_sender
                    .send(VcmiReply::ConnectDialogShowed)
                    .expect("Error in another thread");
            }
            GearReply::NotConnected(reason) => self.main_window.emit("alert", reason).unwrap(),
            GearReply::ProgramNotFound { program_id } => {
                self.main_window.emit("alert", program_id).unwrap()
            }
            _ => unreachable!("Reply {reply:?} is wrong to command Connect"),
        }
    }

    fn connect_to_lobby(&mut self, address: String, username: String) {
        self.lobby_command_sender
            .send(LobbyCommand::Connect(address))
            .expect("Send error");
        self.lobby_command_sender
            .send(LobbyCommand::Greeting(4u8, username, String::new())).expect("Send Error")
    }

    fn process_gui_command(&mut self) {
        match self.gui_command_receiver.recv_timeout(RECV_TIMEOUT) {
            Ok(gui_command) => {
                tracing::debug!("Process Gui Command: {:?}", gui_command);
                match gui_command {
                    GuiCommand::Connect {
                        lobby_address,
                        username,
                        node_address,
                        program_id,
                        password,
                        account_id,
                    } => {
                        self.connect_to_lobby(lobby_address, username);
                        // self.connect_to_node(node_address, program_id, account_id, password);
                    }
                    GuiCommand::Cancel => {
                        // main_window.set_fullscreen(true).unwrap();
                        self.main_window.hide().unwrap();
                        self.vcmi_reply_sender
                            .send(VcmiReply::CanceledDialog)
                            .expect("Panic in another thread");
                        self.need_stop.store(true, Relaxed);
                    }
                    GuiCommand::ExpandLog => {
                        self.log_window.set_size(Size::Logical(LogicalSize::new(0.3, 1.0))).unwrap();
                        // let size = self.log_window.inner_size().unwrap();
                        // const EXPANDED_SIZE: u32 = 600;
                        // let height = match size.height == EXPANDED_SIZE {
                        //     true => 150,
                        //     false => EXPANDED_SIZE,
                        // };
                        // let width = size.width;
                        // self.log_window
                        //     .set_size(Size::Physical(PhysicalSize::new(width, height)))
                        //     .unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        self.log_window.move_window(Position::TopRight).unwrap();
                    }
                }
            }
            Err(e) if e == RecvTimeoutError::Timeout => {}
            Err(e) => {
                tracing::error!("Error in another thread: {}", e);
                self.need_stop.store(true, Relaxed);
            }
        }
    }
}
