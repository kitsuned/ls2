use crate::Error;

#[derive(Debug)]
pub(crate) struct Address {
    pub service: String,
    pub category: String,
    pub method: String,
}

impl Address {
    pub(crate) fn parse(uri: &str) -> Result<Self, Error> {
        let (service, path) = uri
            .strip_prefix("luna://")
            .and_then(|uri| {
                let index = uri.find('/')?;
                Some(uri.split_at(index))
            })
            .ok_or_else(|| Error::invalid("expected a luna://service/category/method URI"))?;
        service_name(service)?;
        let (category, method) = split_path(path)?;
        Ok(Self {
            service: service.to_owned(),
            category: category.to_owned(),
            method: method.to_owned(),
        })
    }
}

pub(crate) fn service_name(name: &str) -> Result<(), Error> {
    if name.split('.').all(segment) {
        Ok(())
    } else {
        Err(Error::invalid("invalid Luna service name"))
    }
}

pub(crate) fn split_path(path: &str) -> Result<(&str, &str), Error> {
    if path.starts_with("//") {
        return Err(Error::invalid("invalid Luna category"));
    }
    let (prefix, method) = path
        .rsplit_once('/')
        .ok_or_else(|| Error::invalid("Luna method path must start with /"))?;
    let category = if prefix.is_empty() { "/" } else { prefix };
    validate_category(category)?;
    validate_method(method)?;
    Ok((category, method))
}

pub(crate) fn validate_category(category: &str) -> Result<(), Error> {
    if category == "/"
        || category
            .strip_prefix('/')
            .is_some_and(|path| path.split('/').all(category_segment))
    {
        Ok(())
    } else {
        Err(Error::invalid("invalid Luna category"))
    }
}

pub(crate) fn validate_method(method: &str) -> Result<(), Error> {
    if segment(method) {
        Ok(())
    } else {
        Err(Error::invalid("invalid Luna method"))
    }
}

pub(crate) fn validate_payload(payload: &str) -> Result<(), Error> {
    if payload.as_bytes().contains(&0) {
        Err(Error::invalid("Luna payload contains a NUL byte"))
    } else if payload.len() >= crate::wire::MAX_BODY_LEN {
        Err(Error::invalid("Luna payload exceeds the wire size limit"))
    } else {
        Ok(())
    }
}

fn segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn category_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_root_and_nested_methods() {
        let root = Address::parse("luna://org.ls2.test/echo").expect("root method");
        assert_eq!((&*root.category, &*root.method), ("/", "echo"));
        let nested = Address::parse("luna://org.ls2.test/foo/bar/echo").expect("nested method");
        assert_eq!((&*nested.category, &*nested.method), ("/foo/bar", "echo"));
    }

    #[test]
    fn rejects_ambiguous_addresses() {
        for uri in [
            "luna:///echo",
            "luna://org.ls2.test//echo",
            "luna://org.ls2.test/echo?raw",
        ] {
            assert!(Address::parse(uri).is_err(), "accepted {uri}");
        }
    }
}
