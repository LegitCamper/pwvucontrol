// Copyright 2021 Tom A. Wagner <tom.a.wagner@protonmail.com>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License version 3 as published by
// the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-only

mod state;

use std::{cell::RefCell, collections::HashMap, rc::Rc, io::{self, BufWriter}};

use gtk::glib::{self, clone};
use log::{debug, info, warn};
use pipewire::{
    link::{Link, LinkChangeMask, LinkListener, LinkState},
    node::{Node, NodeListener},
    prelude::*,
    properties,
    registry::{GlobalObject, Registry},
    spa::{Direction, ForeignDict},
    types::ObjectType,
    Context, Core, MainLoop,
};

use pipewire::spa::pod::deserialize::{
    DeserializeError, DeserializeSuccess, ObjectPodDeserializer, PodDeserialize, PodDeserializer,
    Visitor
};
use pipewire::spa::pod::serialize::{GenError, PodSerialize, PodSerializer, SerializeSuccess};

use crate::{GtkMessage, MediaType, NodeType, PipewireMessage};
use state::{Item, State};

enum ProxyItem {
    Link {
        _proxy: Link,
        _listener: LinkListener,
    },
    Node {
        _proxy: Node,
        _listener: NodeListener,
    },
}

/// The "main" function of the pipewire thread.
pub(super) fn thread_main(
    gtk_sender: glib::Sender<PipewireMessage>,
    pw_receiver: pipewire::channel::Receiver<GtkMessage>,
) {
    let mainloop = MainLoop::new().expect("Failed to create mainloop");
    let context = Context::new(&mainloop).expect("Failed to create context");
    let core = Rc::new(context.connect(None).expect("Failed to connect to remote"));
    let registry = Rc::new(core.get_registry().expect("Failed to get registry"));

    // Keep proxies and their listeners alive so that we can receive info events.
    let proxies = Rc::new(RefCell::new(HashMap::new()));

    let state = Rc::new(RefCell::new(State::new()));

    let _receiver = pw_receiver.attach(&mainloop, {
        clone!(@strong mainloop, @weak core, @weak registry, @strong state, @strong proxies => move |msg| match msg {
            GtkMessage::ToggleLink { port_from, port_to } => toggle_link(port_from, port_to, &core, &registry, &state),
            GtkMessage::Terminate => mainloop.quit(),
            GtkMessage::SetVolume{id, volume} => {
                println!("Got GtkMessage::SetVolume!");

                // Test pod serializing
                let mut buf = io::Cursor::new(Vec::new());
                let p = SomeProps {
                    channel_volumes: Some(vec![volume, volume]),
                    mute: None,
                    volume: None,
                };
                if let Ok(x) = PodSerializer::serialize(&mut buf, &p) {
                    let proxies = proxies.borrow_mut();

                    if let Some(ProxyItem::Node { _proxy: node, .. }) = proxies.get(&id) {
                        node.set_param(pipewire::spa::param::ParamType::Props, 0, &x.0.get_ref());
                    }

                } else {
                    log::error!("Cannot serialize SomeProps");
                }
            },
        })
    });

    let _listener = registry
        .add_listener_local()
        .global(clone!(@strong gtk_sender, @weak registry, @strong proxies, @strong state =>
            move |global| {
                match global.type_ {
                    ObjectType::Node => handle_node(global, &gtk_sender, &registry, &proxies, &state),
                    ObjectType::Port => handle_port(global, &gtk_sender, &state),
                    ObjectType::Link => handle_link(global, &gtk_sender, &registry, &proxies, &state),
                    _ => {
                        // Other objects are not interesting to us
                    }
                }
            }
        ))
        .global_remove(clone!(@strong proxies, @strong state => move |id| {
            if let Some(item) = state.borrow_mut().remove(id) {
                gtk_sender.send(match item {
                    Item::Node { .. } => PipewireMessage::NodeRemoved {id},
                    Item::Port { node_id } => PipewireMessage::PortRemoved {id, node_id},
                    Item::Link { .. } => PipewireMessage::LinkRemoved {id},
                }).expect("Failed to send message");
            } else {
                warn!(
                    "Attempted to remove item with id {} that is not saved in state",
                    id
                );
            }

            proxies.borrow_mut().remove(&id);
        }))
        .register();

    mainloop.run();
}

//#[derive(Default)]
struct SomeProps {
    volume: Option<f32>,
    mute: Option<bool>,
    channel_volumes: Option<Vec<f32>>,
}

const PROPS_KEY_CHANNEL_VOLUMES: u32 = 65544;
const PROPS_KEY_VOLUME: u32 = 65539;
const PROPS_KEY_MUTE: u32 = 65540;

impl<'de> PodDeserialize<'de> for SomeProps {
    fn deserialize(
        deserializer: PodDeserializer<'de>,
    ) -> Result<(Self, DeserializeSuccess<'de>), DeserializeError<&'de [u8]>>
    where
        Self: Sized,
    {
        use pipewire::spa::pod::{Value, ValueArray};
        struct ObjectVisitor;

        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = SomeProps;
            type ArrayElem = std::convert::Infallible;

            fn visit_object(
                &self,
                object_deserializer: &mut ObjectPodDeserializer<'de>,
            ) -> Result<Self::Value, DeserializeError<&'de [u8]>> {
                let mut tmp = SomeProps {
                    channel_volumes: None,
                    volume: None,
                    mute: None,
                };

                while let Some((value, key, _flags)) =
                    object_deserializer.deserialize_property::<Value>()?
                {
                    match key {
                        PROPS_KEY_CHANNEL_VOLUMES => {
                            if let Value::ValueArray(ValueArray::Float(channel_volumes)) = &value {
                                dbg!(channel_volumes);
                                tmp.channel_volumes = Some(channel_volumes.clone());
                            }
                        }
                        PROPS_KEY_VOLUME => {
                            if let Value::Float(volume) = &value {
                                tmp.volume = Some(volume.clone());
                            }
                        }
                        PROPS_KEY_MUTE => {
                            if let Value::Bool(mute) = &value {
                                tmp.mute = Some(mute.clone());
                            }
                        }
                        _ => {}
                    }
                }

                Ok(tmp)
            }
        }

        deserializer.deserialize_object(ObjectVisitor)
    }
}

impl PodSerialize for SomeProps {
    fn serialize<O: io::Write + io::Seek>(
        &self,
        serializer: PodSerializer<O>,
    ) -> Result<SerializeSuccess<O>, GenError> {
        use pipewire::spa::pod::{Value, ValueArray};
        const OBJECT_TYPE_PROPS: u32 = 262146;
        const OBJECT_ID_PROPS: u32 = 2;

        let mut object_serializer = serializer.serialize_object(OBJECT_TYPE_PROPS, OBJECT_ID_PROPS)?;
        if let Some(channel_volumes) = &self.channel_volumes {
            object_serializer.serialize_property(
                PROPS_KEY_CHANNEL_VOLUMES,
                &Value::ValueArray(ValueArray::Float(channel_volumes.clone())),
                pipewire::spa::pod::PropertyFlags::empty())?;
        }

        if let Some(volume) = &self.volume {
            object_serializer.serialize_property(
                PROPS_KEY_VOLUME,
                &Value::Float(*volume),
                pipewire::spa::pod::PropertyFlags::empty())?;
        }

        if let Some(mute) = &self.mute {
            object_serializer.serialize_property(
                PROPS_KEY_MUTE,
                &Value::Bool(*mute),
                pipewire::spa::pod::PropertyFlags::empty())?;
        }

        object_serializer.end()
    }
}

/// Handle a new node being added
fn handle_node(
    node: &GlobalObject<ForeignDict>,
    sender: &glib::Sender<PipewireMessage>,
    registry: &Rc<Registry>,
    proxies: &Rc<RefCell<HashMap<u32, ProxyItem>>>,
    state: &Rc<RefCell<State>>,
) {
    debug!(
        "New node (id:{}) appeared, setting up node listener.",
        node.id
    );

    let node_id = node.id.clone();

    let proxy: pipewire::node::Node = registry.bind(node).expect("Failed to bind to node proxy");

    let listener = proxy
        .add_listener_local()
        .info(clone!(@strong proxies => move |ni| {
            if ni.change_mask().contains(pipewire::node::NodeChangeMask::PARAMS) {
                let proxies = proxies.borrow_mut();

                if let Some(ProxyItem::Node { _proxy: hi, .. }) = proxies.get(&ni.id()) {
                    hi.enum_params(0, Some(pipewire::spa::param::ParamType::Props), 0, u32::MAX);
                }
            }
        }))
        .param(clone!(@strong sender => move |seq, id, start, num, param| {
            use pipewire::spa::pod::{Value, ValueArray};

            debug!("New param! {seq} {} {start} {num}", id.0);

            let (_, x) = PodDeserializer::deserialize_from::<SomeProps>(&param).expect("Error deserializing into Value");
            if let Some(channel_volumes) = x.channel_volumes {
                println!("Channel volumes: {:?}", channel_volumes);
                sender.send(PipewireMessage::NodeParam{
                    id: node_id,
                    param: crate::ParamType::ChannelVolumes(channel_volumes.clone())})
                    .expect("Failed to send ChannelVolumes message");
            }



            /*
            let (_, object) = PodDeserializer::deserialize_from::<Value>(&param).expect("Error deserializing into Value");
            if let Value::Object(obj) = object {
                if let Some(channel_volumes_prop) = obj.properties.iter().find(|x| {x.key == 65544}) {
                    println!("Channel volumes: {:?}", channel_volumes_prop.value);
                    if let Value::ValueArray(ValueArray::Float(channel_volumes)) = &channel_volumes_prop.value {
                        sender.send(PipewireMessage::NodeParam{
                            id: node_id,
                            param: crate::ParamType::ChannelVolumes(channel_volumes.clone())})
                            .expect("Failed to send ChannelVolumes message");
                    }
                }
                if let Some(volume_prop) = obj.properties.iter().find(|x| {x.key == 65539}) {
                    println!("Main volume: {:?}", volume_prop.value);
                    if let Value::Float(volume) = &volume_prop.value {
                        sender.send(PipewireMessage::NodeParam{
                            id: node_id,
                            param: crate::ParamType::Volume(volume.clone())})
                            .expect("Failed to send Volume message");
                    }
                }
                if let Some(mute_prop) = obj.properties.iter().find(|x| {x.key == 65540}) {
                    println!("Main volume: {:?}", mute_prop.value);
                    if let Value::Bool(mute) = &mute_prop.value {
                        sender.send(PipewireMessage::NodeParam{
                            id: node_id,
                            param: crate::ParamType::Mute(mute.clone())})
                            .expect("Failed to send Mute message");
                    }
                }
            }
            */
        }))
        .register();


    proxies.borrow_mut().insert(
        node.id,
        ProxyItem::Node {
            _proxy: proxy,
            _listener: listener,
        },
    );

    let props = node
        .props
        .as_ref()
        .expect("Node object is missing properties");

    // Get the nicest possible name for the node, using a fallback chain of possible name attributes.
    let name = String::from(
        props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
            .or_else(|| props.get("node.name"))
            .unwrap_or_default(),
    );

    // FIXME: Instead of checking these props, the "EnumFormat" parameter should be checked instead.
    let media_type = props.get("media.class").and_then(|class| {
        if class.contains("Audio") {
            Some(MediaType::Audio)
        } else if class.contains("Video") {
            Some(MediaType::Video)
        } else if class.contains("Midi") {
            Some(MediaType::Midi)
        } else {
            None
        }
    });

    let media_class = |class: &str| {
        if class.contains("Sink") {
            Some(NodeType::Sink)
        } else if class.contains("Output") {
            Some(NodeType::Output)
        } else if class.contains("Input") {
            Some(NodeType::Input)
        } else if class.contains("Source") {
            Some(NodeType::Source)
        } else {
            None
        }
    };

    let node_type = props
        .get("media.category")
        .and_then(|class| {
            if class.contains("Duplex") {
                None
            } else {
                props.get("media.class").and_then(media_class)
            }
        })
        .or_else(|| props.get("media.class").and_then(media_class));

    state.borrow_mut().insert(
        node.id,
        Item::Node {
            // widget: node_widget,
            media_type,
        },
    );

    sender
        .send(PipewireMessage::NodeAdded {
            id: node.id,
            name,
            node_type,
        })
        .expect("Failed to send message");
}

/// Handle a new port being added
fn handle_port(
    port: &GlobalObject<ForeignDict>,
    sender: &glib::Sender<PipewireMessage>,
    state: &Rc<RefCell<State>>,
) {
    let props = port
        .props
        .as_ref()
        .expect("Port object is missing properties");
    let name = props.get("port.name").unwrap_or_default().to_string();
    let node_id: u32 = props
        .get("node.id")
        .expect("Port has no node.id property!")
        .parse()
        .expect("Could not parse node.id property");
    let direction = if matches!(props.get("port.direction"), Some("in")) {
        Direction::Input
    } else {
        Direction::Output
    };

    // Find out the nodes media type so that the port can be colored.
    let media_type = if let Some(Item::Node { media_type, .. }) = state.borrow().get(node_id) {
        media_type.to_owned()
    } else {
        warn!("Node not found for Port {}", port.id);
        None
    };

    // Save node_id so we can delete this port easily.
    state.borrow_mut().insert(port.id, Item::Port { node_id });

    sender
        .send(PipewireMessage::PortAdded {
            id: port.id,
            node_id,
            name,
            direction,
            media_type,
        })
        .expect("Failed to send message");
}

/// Handle a new link being added
fn handle_link(
    link: &GlobalObject<ForeignDict>,
    sender: &glib::Sender<PipewireMessage>,
    registry: &Rc<Registry>,
    proxies: &Rc<RefCell<HashMap<u32, ProxyItem>>>,
    state: &Rc<RefCell<State>>,
) {
    debug!(
        "New link (id:{}) appeared, setting up info listener.",
        link.id
    );

    let proxy: Link = registry.bind(link).expect("Failed to bind to link proxy");
    let listener = proxy
        .add_listener_local()
        .info(clone!(@strong state, @strong sender => move |info| {
            debug!("Received link info: {:?}", info);

            let id = info.id();

            let mut state = state.borrow_mut();
            if let Some(Item::Link { .. }) = state.get(id) {
                // Info was an update - figure out if we should notify the gtk thread
                if info.change_mask().contains(LinkChangeMask::STATE) {
                    sender.send(PipewireMessage::LinkStateChanged {
                        id,
                        active: matches!(info.state(), LinkState::Active)
                    }).expect("Failed to send message");
                }
                // TODO -- check other values that might have changed
            } else {
                // First time we get info. We can now notify the gtk thread of a new link.
                let node_from = info.output_node_id();
                let port_from = info.output_port_id();
                let node_to = info.input_node_id();
                let port_to = info.input_port_id();

                state.insert(id, Item::Link {
                    port_from, port_to
                });

                sender.send(PipewireMessage::LinkAdded {
                    id,
                    node_from,
                    port_from,
                    node_to,
                    port_to,
                    active: matches!(info.state(), LinkState::Active)
                }).expect(
                    "Failed to send message"
                );
            }
        }))
        .register();

    proxies.borrow_mut().insert(
        link.id,
        ProxyItem::Link {
            _proxy: proxy,
            _listener: listener,
        },
    );
}

/// Toggle a link between the two specified ports.
fn toggle_link(
    port_from: u32,
    port_to: u32,
    core: &Rc<Core>,
    registry: &Rc<Registry>,
    state: &Rc<RefCell<State>>,
) {
    let state = state.borrow_mut();
    if let Some(id) = state.get_link_id(port_from, port_to) {
        info!("Requesting removal of link with id {}", id);

        // FIXME: Handle error
        registry.destroy_global(id);
    } else {
        info!(
            "Requesting creation of link from port id:{} to port id:{}",
            port_from, port_to
        );

        let node_from = state
            .get_node_of_port(port_from)
            .expect("Requested port not in state");
        let node_to = state
            .get_node_of_port(port_to)
            .expect("Requested port not in state");

        if let Err(e) = core.create_object::<Link, _>(
            "link-factory",
            &properties! {
                "link.output.node" => node_from.to_string(),
                "link.output.port" => port_from.to_string(),
                "link.input.node" => node_to.to_string(),
                "link.input.port" => port_to.to_string(),
                "object.linger" => "1"
            },
        ) {
            warn!("Failed to create link: {}", e);
        }
    }
}