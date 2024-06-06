pub use crate::transport::MESSAGE_LENGTH_PAYLOAD_MAX;

#[macro_export]
#[doc(hidden)]
macro_rules! mcu_message_impl_oid_check {
    ($ty_name:ident) => {
        impl $crate::messages::WithoutOid for $ty_name {}
    };
    ($ty_name:ident, oid $(, $args:ident)*) => {
        impl $crate::messages::WithOid for $ty_name {}
    };
    ($ty_name:ident, $arg:ident $(, $args:ident)*) => {
        $crate::mcu_message_impl_oid_check!($ty_name $(, $args)*);
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! mcu_message_impl {
    ($ty_name:ident, $cmd_name:literal = $cmd_id:expr $(, $arg:ident : $kind:ty)*) => {
        paste::paste! {
            #[derive(Debug)]
            struct $ty_name;

            #[allow(dead_code)]
            impl $ty_name {
                #[allow(clippy::extra_unused_lifetimes)]
                pub fn encode<'a>($($arg: <$kind as $crate::encoding::Borrowable>::Borrowed<'a>, )*) -> $crate::messages::EncodedMessage<Self> {
                    let payload = [<$ty_name Data>] {
                        $($arg,)*
                        _lifetime: Default::default(),
                    }.encode();
                    $crate::messages::EncodedMessage {
                        payload,
                        _message_kind: Default::default(),
                    }
                }

                pub fn decode<'a>(input: &mut &'a [u8]) -> Result<[<$ty_name Data>]<'a>, $crate::encoding::MessageDecodeError> {
                    [<$ty_name Data>]::decode(input)
                }
            }

            #[allow(dead_code)]
            struct [<$ty_name Data>]<'a> {
                // $(pub $arg: $kind,)*
                $(pub $arg: <$kind as $crate::encoding::Borrowable>::Borrowed<'a>,)*

                _lifetime: std::marker::PhantomData<&'a ()>,
            }

            #[allow(dead_code)]
            impl<'a> [<$ty_name Data>]<'a> {
                fn encode(&self) -> $crate::messages::FrontTrimmableBuffer {
                    use $crate::encoding::Writable;
                    let mut buf = Vec::with_capacity($crate::macros::MESSAGE_LENGTH_PAYLOAD_MAX);
                    buf.push(0);
                    buf.push(0);
                    $(self.$arg.write(&mut buf);)*
                    $crate::messages::FrontTrimmableBuffer { content: buf, offset: 0 }
                }

                fn decode(input: &mut &'a [u8]) -> Result<Self, $crate::encoding::MessageDecodeError> {
                    $(let $arg = $crate::encoding::Readable::read(input)?;)*
                    Ok(Self { $($arg,)* _lifetime: Default::default() })
                }

            }

            impl<'a> std::fmt::Debug for [<$ty_name Data>]<'a> {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    let mut ds = f.debug_struct(stringify!([<$ty_name Data>]));
                    $( let ds = ds.field(stringify!($arg), &self.$arg); )*
                    ds.finish()
                }
            }

            #[derive(Clone)]
            #[allow(dead_code)]
            struct [<$ty_name DataOwned>] {
                $(pub $arg: $kind,)*
            }

            impl<'a> std::convert::From<[<$ty_name Data>]<'a>> for [<$ty_name DataOwned>] {
                fn from(value: [<$ty_name Data>]) -> Self {
                    Self {
                        $($arg: $crate::encoding::Borrowable::from_borrowed(value.$arg),)*
                    }
                }
            }

            impl std::fmt::Debug for [<$ty_name DataOwned>] {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    let mut ds = f.debug_struct(stringify!([<$ty_name DataOwned>]));
                    $( let ds = ds.field(stringify!($arg), &self.$arg); )*
                    ds.finish()
                }
            }

            #[allow(dead_code)]
            impl $crate::messages::Message for $ty_name {
                type Pod<'a> = [<$ty_name Data>]<'a>;
                type PodOwned = [<$ty_name DataOwned>];

                fn get_id(dict: Option<&$crate::dictionary::Dictionary>) -> Option<u16> {
                    $cmd_id.or_else(|| dict.and_then(|dict| dict.message_ids.get($cmd_name).copied()))
                }

                fn get_name() -> &'static str {
                    $cmd_name
                }

                fn decode<'a>(input: &mut &'a [u8]) -> Result<Self::Pod<'a>, $crate::encoding::MessageDecodeError> {
                    Self::decode(input)
                }

                fn fields<'a>() -> Vec<(&'static str, $crate::encoding::FieldType)> {
                    vec![
                        $( ( stringify!($arg), <$kind as $crate::encoding::ToFieldType>::as_field_type() ), )*
                    ]
                }
            }

            $crate::mcu_message_impl_oid_check!($ty_name $(, $arg)*);
        }
    };
}

/// Declare a host → device reply message
///
/// Declares the format of a message sent from host to device. The general format is as follows:
///
/// ```text
/// mcu_command!(<struct name>, "<command name>"( = <id>) [, arg: type, ..]);
/// ```
///
/// A struct with the given name will be defined, with relevant interfaces. The message name and
/// arguments will be matched to the MCU at runtime. The struct name has no restrictions, but it is
/// recommended to pick a SnakeCased version of the command name.
///
/// Optionally an `id` can be directly specified. Generally this is not needed. When not specified,
/// it will be automatically inferred at runtime and matched to the dictionary retrieved from the
/// target MCU.
///
/// Arguments can be specified, they will be mapped to the relevant Klipper argument types. The
/// supported types and mappings are as follows:
///
/// | Rust type  | Format string |
/// |------------|---------------|
/// | `u32`      | `%u`          |
/// | `i32`      | `%i`          |
/// | `u16`      | `%hu`         |
/// | `i16`      | `%hi`         |
/// | `u8`       | `%c`          |
/// | `&'a [u8]` | `%.*s`, `%*s` |
/// | `&'a str`  | `%s`          |
///
/// Note that the buffer types take a lifetime. This must always be `'a`.
///
/// # Examples
///
/// ```ignore
/// // This defines 'config_endstop oid=%c pin=%c pull_up=%c'
/// mcu_command!(ConfigEndstop, "config_endstop", oid: u8, pin: u8, pull_up: u8);
/// ```
#[macro_export]
macro_rules! mcu_command {
    ($ty_name:ident, $cmd_name:literal $(, $arg:ident : $kind:ty)* $(,)?) => {
        $crate::mcu_message_impl!($ty_name, $cmd_name = None $(, $arg: $kind)*);
    };
    ($ty_name:ident, $cmd_name:literal = $cmd_id:literal $(, $arg:ident : $kind:ty)* $(,)?) => {
        $crate::mcu_message_impl!($ty_name, $cmd_name = Some($cmd_id) $(, $arg: $kind)*);
    };
}

/// Declare a device → host reply message
///
/// Declares the format of a message sent from device to host. The general format is as follows:
///
/// ```text
/// mcu_reply!(<struct name>, "<reply name>"( = <id>) [, arg: type, ..]);
/// ```
///
/// For more information on the various fields, see the documentation for [mcu_command].
///
/// # Examples
///
/// ```ignore
/// // This defines 'config_endstop oid=%c pin=%c pull_up=%c'
/// mcu_reply!(Uptime, "uptime", high: u32, clock: u32);
/// ```
#[macro_export]
macro_rules! mcu_reply {
    ($ty_name:ident, $cmd_name:literal $(, $arg:ident : $kind:ty)* $(,)?) => {
        $crate::mcu_message_impl!($ty_name, $cmd_name = None $(, $arg: $kind)*);
    };
    ($ty_name:ident, $cmd_name:literal = $cmd_id:literal $(, $arg:ident : $kind:ty)* $(,)?) => {
        $crate::mcu_message_impl!($ty_name, $cmd_name = Some($cmd_id) $(, $arg: $kind)*);
    };
}
