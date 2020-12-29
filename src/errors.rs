macro_rules! gen_error_type {
    ($name:ident, $descr:expr) => {
        #[derive(Debug)]
        pub struct $name;

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, $descr)
            }
        }

        impl ::std::error::Error for $name {
            fn source(&self) -> ::std::option::Option<&(dyn ::std::error::Error + 'static)> {
                None
            }
        }
    };
}

gen_error_type!(DnsResolutionFailed, "The DNS resolution failed");
gen_error_type!(Timeout, "The query timed out");
gen_error_type!(ChannelError, "A send or receive operation failed");
