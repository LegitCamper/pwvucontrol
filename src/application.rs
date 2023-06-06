/* application.rs
 *
 * Copyright 2023 Nicolai Syvertsen
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use std::cell::RefCell;

use gtk::prelude::*;
use adw::subclass::prelude::*;
use gtk::{
    gio,
    glib::{self, clone, Continue, Receiver},
};
use log::info;

use crate::{
    GtkMessage, MediaType, NodeType, PipewireLink, PipewireMessage, pwnodeobject::PwNodeObject,
};

use crate::config::VERSION;
use crate::PwvucontrolWindow;
use pipewire::{channel::Sender, spa::Direction};

mod imp {
    use super::*;
    use once_cell::unsync::OnceCell;

    #[derive(Default)]
    pub struct PwvucontrolApplication {
        pub(super) pw_sender: OnceCell<RefCell<Sender<GtkMessage>>>,
        pub(super) window: OnceCell<PwvucontrolWindow>,
        pub(super) signalblockers: RefCell<std::collections::HashMap<u32, glib::SignalHandlerId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PwvucontrolApplication {
        const NAME: &'static str = "PwvucontrolApplication";
        type Type = super::PwvucontrolApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for PwvucontrolApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.setup_gactions();
            obj.set_accels_for_action("app.quit", &["<primary>q"]);
        }
    }

    impl ApplicationImpl for PwvucontrolApplication {
        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let window = self
            .window
            .get()
            .expect("Should always be initialized in gio_application_startup");

/*
            let application = self.obj();
            // Get the current window or create one if necessary
            let window = if let Some(window) = application.active_window() {
                window
            } else {
                let window = PwvucontrolWindow::new(&*application);
                window.set_title(Some("hi"));
                {
                    self.window = Some(RefCell::from(window));
                }
                window.upcast()
            };
 */

            // Ask the window manager/compositor to present the window
            window.present();
        }

        fn startup(&self) {
            self.parent_startup();
    
            let window = PwvucontrolWindow::new(&self.obj());
            self.window
                .set(window)
                .expect("Failed to initialize application window");
        }
    }

    impl GtkApplicationImpl for PwvucontrolApplication {}
    impl AdwApplicationImpl for PwvucontrolApplication {}
}

glib::wrapper! {
    pub struct PwvucontrolApplication(ObjectSubclass<imp::PwvucontrolApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl PwvucontrolApplication {
    pub(super) fn new(
        gtk_receiver: Receiver<PipewireMessage>,
        pw_sender: Sender<GtkMessage>,
    ) -> Self {
        let app:PwvucontrolApplication = glib::Object::builder()
            .property("application-id", "com.saivert.pwvucontrol")
            .property("flags", &gio::ApplicationFlags::empty())
            .property("resource-base-path", &"/com/saivert/pwvucontrol")
            .build();
        
        let imp = app.imp();
        imp.pw_sender
            .set(RefCell::new(pw_sender))
            // Discard the returned sender, as it does not implement `Debug`.
            .map_err(|_| ())
            .expect("pw_sender field was already set");

        // React to messages received from the pipewire thread.
        gtk_receiver.attach(
            None,
            clone!(
                @weak app => @default-return Continue(true),
                move |msg| {
                    
                    match msg {
                        PipewireMessage::NodeAdded{ id, name, node_type } => app.add_node(id, name.as_str(), node_type),
                        PipewireMessage::NodeRemoved{ id } => app.remove_node(id),
                        PipewireMessage::NodeParam{id, param} => app.node_param(id, param),
                        _ => {}
                    };
                    Continue(true)
                }
            ),
        );

        app
    }
    
    fn node_param(&self, id: u32, param: crate::ParamType) {
        use crate::ParamType::*;
        if let Some(x) = self.imp().window.get() {
            
            match param {
                Volume(v) => {
                    _ = x.imp().nodemodel.update_node(id, |node| {
                        //node.set_volume(v);
                   });
                },
                Mute(m) => {
                   _ = x.imp().nodemodel.update_node(id, |node| {
                        node.set_mute(m);
                   });
                },
                ChannelVolumes(cv) => {
                    if let Ok(nodeobj) = x.imp().nodemodel.get_node(id) {
                        if let Some(volume) = cv.get(0) {
                            if let Some(sigid) = self.imp().signalblockers.borrow().get(&id){
                                nodeobj.block_signal(sigid);
                                nodeobj.set_volume(*volume);
                                nodeobj.unblock_signal(sigid);
                            }
                        }
                    }
                    /*
                    _ = x.imp().nodemodel.update_node(id, |node| {
                        if (cv.len() > 0) {
                            //node.imp().set_channel_volumes_vec(&cv);
                            node.set_volume(cv.iter().sum::<f32>() / cv.len() as f32);
                        } else {
                            log::error!("cv is 0");
                        }
                   });
                   */
                },
            }
        }
    }

    /// Add a new node to the view.
    fn add_node(&self, id: u32, name: &str, node_type: Option<NodeType>) {
        info!("Adding node: id {}", id);

        if let Some(x) = node_type {
            if matches!(x, NodeType::Output) {
                if let Some(x) = self.imp().window.get() {
                    let y = &PwNodeObject::new(id, name);

                    let sender = self
                    .imp()
                    .pw_sender
                    .get()
                    .expect("pw_sender not set")
                    .borrow_mut();

                    let t = y.connect_notify_local(Some("volume"), clone!(@strong sender => move |obj, paramspec| {
                        if let Ok(volume) = obj.property_value("volume").get::<f32>() {
                            sender.send(GtkMessage::SetVolume{id, volume}).expect("Unable to send set volume message from app.");
                        }
                    }));

                    self.imp().signalblockers.borrow_mut().insert(id, t);
                    x.imp().nodemodel.append(y);
                }
                return;
            }
        }

    }

    fn remove_node(&self, id: u32) {
        info!("Remove node: id {}", id);

        if let Some(x) = self.imp().window.get() {
            x.imp().nodemodel.remove(id);
        }
    }


    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        self.add_action_entries([quit_action, about_action])
    }

    fn show_about(&self) {
        let window = self.active_window().unwrap();
        let about = adw::AboutWindow::builder()
            .transient_for(&window)
            .application_name("pwvucontrol")
            .application_icon("com.saivert.pwvucontrol")
            .developer_name("Nicolai Syvertsen")
            .version(VERSION)
            .developers(vec!["Nicolai Syvertsen"])
            .copyright("© 2023 Nicolai Syvertsen")
            .build();

        about.present();
    }
}