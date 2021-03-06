mod types;

use types::PostBufferError;

use crate::http::http_core::HttpStatus;

use std::fs::{self, OpenOptions};

use std::io::{self, Write};

use std::path::PathBuf;

use core::ptr::copy;

use boyer_moore_magiclen::BMByte;

use crate::http::boyer_moore::{find_body_start, types::BMBuf};

const POST_BUFFER_SIZE: usize = 32 * 1024 * 1024;

#[derive(PartialEq)]
enum PostRequestState {
    AwaitingFirstBody,
    AwaitingBody,
    AwaitingMeta,
    DiscardingData,
}

pub struct PostBuffer {
    fill_location: usize,
    buffer: Box<[u8]>,
    post_delimeter: BMByte,
    post_delimeter_string: String,
    current_filename: Option<PathBuf>,
    current_file: Option<fs::File>,
    state: PostRequestState,
    dir: PathBuf,
    parse_idx: usize,
    queued_error: PostBufferError,
    new_files: Vec<String>,
    total_written: usize,
    size_limit: usize,
}

impl PostBuffer {
    pub fn new(
        dir: PathBuf,
        delim: BMByte,
        delim_str: String,
        slice: &[u8],
        size_limit: usize,
    ) -> PostBuffer {
        let mut pb = PostBuffer {
            buffer: {
                let mut v: Vec<u8> = Vec::with_capacity(POST_BUFFER_SIZE);
                unsafe {
                    v.set_len(POST_BUFFER_SIZE);
                }
                v.into_boxed_slice()
            },
            fill_location: slice.len(),
            post_delimeter: delim,
            post_delimeter_string: delim_str,
            current_filename: None,
            current_file: None,
            state: PostRequestState::AwaitingFirstBody,
            dir: dir,
            parse_idx: 0,
            queued_error: PostBufferError::no_error(),
            new_files: Vec::<String>::new(),
            total_written: 0,
            size_limit: size_limit,
        };
        pb.buffer[..pb.fill_location].clone_from_slice(slice);
        pb.total_written += pb.fill_location;

        pb
    }

    pub fn get_new_files(&self) -> &Vec<String> { &self.new_files }

    pub fn read_into_buffer<T>(&mut self, readable: &mut T) -> Result<usize, io::Error>
    where
        T: io::Read,
    {
        let read = readable.read(&mut self.buffer[self.fill_location..])?;
        self.fill_location += read;
        Ok(read)
    }

    fn find_next_delim(&self, start: usize) -> Option<usize> {
        let vec = self
            .post_delimeter
            .find_in(BMBuf(&self.buffer[start..self.fill_location]), 1);
        if vec.len() < 1 {
            None
        } else {
            Some(vec[0] + start)
        }
    }

    fn write_to_file_final(&mut self, limit: usize) -> Result<(), PostBufferError> {
        if self.current_file.is_none() {
            return Err(PostBufferError::server_error(
                "Attempted to write to a file before opening it.".to_string(),
            ));
        }

        if self.fill_location < limit {
            return Err(PostBufferError::server_error(
                "Asked to write more than avaiable".to_string(),
            ));
        }

        self.write_and_shuffle(limit)?;

        self.current_file = None;

        Ok(())
    }

    fn shuffle(&mut self, remain: usize) {
        // Shuffle
        unsafe {
            /*
            if amount_remaining > self.parse_idx {
                panic!("About to do a ptr::copy_nonoverlapping call on aliased memory locations.");
            }
            */
            copy(
                self.buffer.as_ptr().offset(self.parse_idx as isize),
                self.buffer.as_mut_ptr(),
                remain,
            );
            /*
            // A safe version (if this copy could never alias) would be:
            &self.buffer[..amount_remaining]
                .clone_from_slice(&self.buffer[self.parse_idx..self.fill_location]);
            */
        }

        self.parse_idx = 0;
        self.fill_location = remain;
    }

    fn write_and_shuffle(&mut self, up_to: usize) -> Result<(), PostBufferError> {
        if up_to <= self.parse_idx {
            // Need to read more before this can occur
            return Ok(());
        }

        if self.size_limit > 0 && self.total_written + up_to - self.parse_idx > self.size_limit {
            return Err(PostBufferError::new(
                HttpStatus::PayloadTooLarge,
                format!("Upload size limit of {} bytes exceeded", self.size_limit),
            ));
        }

        let written = match self
            .current_file
            .as_ref()
            .unwrap()
            .write(&self.buffer[self.parse_idx..up_to])
        {
            Ok(size) => size,
            Err(_) => {
                return Err(PostBufferError::server_error(
                    "Error writing to file.".to_string(),
                ));
            }
        };

        self.parse_idx += written;
        self.total_written += written;

        let amount_remaining: usize = self.fill_location - self.parse_idx;

        self.shuffle(amount_remaining);

        Ok(())
    }

    fn send_buffer_data_to_file(&mut self, limit: usize) -> Result<(), PostBufferError> {
        if self.current_file.is_none() {
            return Err(PostBufferError::server_error(
                "Attempted to write to a file before opening it.".to_string(),
            ));
        }

        if limit < self.post_delimeter_string.len() {
            return Err(PostBufferError::server_error(
                "Not enough data to write anything.".to_string(),
            ));
        }

        // Don't write the last few bytes. An incomplete delimeter could be here.
        let real_limit: usize = limit - self.post_delimeter_string.len();

        self.write_and_shuffle(real_limit)?;

        Ok(())
    }

    /* This function implements the worst aspect of HTTP POST:
     * before the browser will accept our response, we must first read the entire
     * request body from the browser.
     * To do this, when an error is detected, we switch the internal state of
     * PostBuffer to start discarding all data, but tell the rest of the server
     * that nothing has gone wrong.
     * We pre-prepare the error message to be sent, but only write its contents
     * when the ConnectionState is switched to WritingResponse, which occurs
     * when we've reached the end of the sent file.
     */
    /* If it is desirable to simply have bad POST requests get a TCP RST
     * with no error message (although one is sent before the reset, browsers
     * won't display it), call `handle_new_data()` directly.
     */
    pub fn handle_new_data_queue_error(&mut self) -> Result<bool, PostBufferError> {
        loop {
            match self.handle_new_data() {
                Ok(done) => {
                    if done && self.state == PostRequestState::DiscardingData {
                        return Err(self.queued_error.clone());
                    } else {
                        return Ok(done);
                    }
                }
                Err(s) => {
                    self.state = PostRequestState::DiscardingData;
                    self.queued_error.add_error(&s);
                }
            }
        }
    }

    // `handle_new_data_raw` wrapper that will delete the current file
    // when an error occurs.
    pub fn handle_new_data(&mut self) -> Result<bool, PostBufferError> {
        let mut res = self.handle_new_data_raw();
        match res {
            Ok(_) => {}
            Err(ref mut e) => {
                if let Some(ref s) = self.current_filename {
                    if let Err(io_e) = fs::remove_file(s) {
                        e.add_error(&PostBufferError::server_error(format!("{:?}", io_e)));
                    }
                    self.current_filename = None;
                    self.current_file = None; // close if open
                }
            }
        };

        res
    }

    pub fn handle_new_data_raw(&mut self) -> Result<bool, PostBufferError> {
        // Where parsing should begin
        loop {
            match self.state {
                PostRequestState::DiscardingData => {
                    let new_idx = match self.find_next_delim(self.parse_idx) {
                        None => {
                            // Cannot find the delimeter, so keep reading. This is good
                            // for slow connections. If we can't find the delimeter in 32M
                            // eventually `read` will return 0 and the connection will be
                            // aborted.
                            self.shuffle(self.post_delimeter_string.len());
                            return Ok(false);
                        }
                        Some(idx) => idx + self.post_delimeter_string.len(),
                    };
                    if self.fill_location - new_idx < 2 {
                        self.shuffle(self.post_delimeter_string.len() + 2);
                        // Need to get \r\n or --
                        return Ok(false);
                    }

                    if self.buffer[new_idx] == '-' as u8 && self.buffer[new_idx + 1] == '-' as u8 {
                        // Read final delimeter, so we're done.
                        return Ok(true);
                    }

                    self.shuffle(self.post_delimeter_string.len());
                }
                PostRequestState::AwaitingFirstBody => {
                    let new_idx = match self.find_next_delim(self.parse_idx) {
                        None => {
                            // Cannot find the delimeter, so keep reading. This is good
                            // for slow connections. If we can't find the delimeter in 32M
                            // eventually `read` will return 0 and the connection will be
                            // aborted.
                            return Ok(false);
                        }
                        Some(idx) => idx + self.post_delimeter_string.len(),
                    };
                    if self.fill_location - new_idx < 2 {
                        // Need to get \r\n or --
                        return Ok(false);
                    }

                    if self.buffer[new_idx] == '-' as u8 && self.buffer[new_idx + 1] == '-' as u8 {
                        // Read final delimeter, so we're done.
                        return Ok(true);
                    }

                    self.parse_idx = new_idx + 2; // Skip \r\n

                    self.state = PostRequestState::AwaitingMeta;
                }
                PostRequestState::AwaitingBody => {
                    let end = match self.find_next_delim(self.parse_idx) {
                        None => {
                            self.send_buffer_data_to_file(self.fill_location)?;
                            return Ok(false);
                        }
                        Some(idx) => {
                            if idx < 2 {
                                return Err(PostBufferError::new(
                                    HttpStatus::BadRequest,
                                    "No CRLF before delimeter. Malformed request.".to_string(),
                                ));
                            }
                            idx - 2
                        }
                    };

                    self.write_to_file_final(end)?;

                    self.state = PostRequestState::AwaitingFirstBody;
                }
                PostRequestState::AwaitingMeta => {
                    let body_start =
                        match find_body_start(&self.buffer[self.parse_idx..self.fill_location]) {
                            Some(idx) => idx + self.parse_idx,
                            None => {
                                // Waiting for more metadata
                                return Ok(false);
                            }
                        };

                    let meta = &self.buffer[self.parse_idx..body_start];
                    let meta_str = String::from_utf8_lossy(meta).to_string();

                    let mut info: &str = "";

                    for line in meta_str.split("\r\n") {
                        let (head, val) = line.split_at(match line.find(":") {
                            Some(idx) => idx + 1,
                            None => {
                                continue;
                            }
                        });
                        if head.to_lowercase() == "content-disposition:" {
                            info = val;
                            break;
                        }
                    }
                    if info == "" {
                        return Err(PostBufferError::new(
                            HttpStatus::UnprocessableEntity,
                            format!("Did not receive a Content-Disposition:\n{}", meta_str),
                        ));
                    }

                    let mut filename: &str = "";
                    for kv in info.split(";") {
                        if let Some(idx) = kv.find("=") {
                            let (k, v) = kv.split_at(idx);
                            if k.trim_start() == "filename" {
                                // 1.. to discard '='
                                filename = &v[1..].trim();
                                break;
                            }
                        }
                    }

                    if filename == "" {
                        return Err(PostBufferError::new(
                            HttpStatus::UnprocessableEntity,
                            "Could not find attribute with a filename".to_string(),
                        ));
                    }

                    if filename.contains("/") {
                        return Err(PostBufferError::new(
                            HttpStatus::UnprocessableEntity,
                            format!("Invalid filename: {}", filename),
                        ));
                    }

                    if filename.starts_with("\"") {
                        filename = &filename[1..filename.len() - 1];
                    }

                    self.new_files.push(filename.to_string());

                    let real_filename = self.dir.join(filename);

                    self.current_file = Some(
                        match OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&real_filename)
                        {
                            Ok(f) => f,
                            _ => {
                                return Err(PostBufferError::server_error(
                                    "Could not open file for writing. If the file already exists, \
                                     please use a different name."
                                        .to_string(),
                                ));
                            }
                        },
                    );

                    self.current_filename = Some(real_filename);

                    self.state = PostRequestState::AwaitingBody;

                    self.parse_idx = body_start;
                }
            }
        }
    }
}
