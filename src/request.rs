use std::collections::HashMap;

pub struct Header {
    key: String,
    values: Vec<String>,
}

impl Header {
    pub fn new(key: String, value: String) -> Self {
        let mut values = Vec::new();
        values.push(value);
        Self {
            key,
            values,
        }
    }

    pub fn with_values(key: String, values: Vec<String>) -> Self {
        Self {
            key,
            values,
        }
    }
}

pub struct Request {
    headers: HashMap<String, Vec<String>>,
}

impl Request {
    fn new() -> Self {
        //
    }

    pub fn set_header() {}
}
