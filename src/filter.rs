use std::collections::HashMap;

use mustache;

use crate::config::*;
use crate::error::Error;

pub struct FilterPipelineBuilder {
    data: mustache::Data,
    filters: Vec<String>,
    content_type: String,
}

impl FilterPipelineBuilder {
    pub fn new(data: mustache::Data) -> Self {
        FilterPipelineBuilder {
            data,
            filters: Vec::new(),
            content_type: "video/MP2T".to_string(),
        }
    }

    pub fn build(self) -> (Vec<String>, String) {
        (self.filters, self.content_type)
    }

    pub fn add_pre_filters(
        &mut self,
        pre_filters: &HashMap<String, FilterConfig>,
        names: &Vec<String>
    ) -> Result<(), Error> {
        for name in names.iter() {
            if pre_filters.contains_key(name) {
                self.add_pre_filter(&pre_filters[name], name)?;
            } else {
                log::warn!("No such pre-filter: {}", name);
            }
        }
        Ok(())
    }

    pub fn add_pre_filter(
        &mut self,
        config: &FilterConfig,
        name: &str,
    ) -> Result<(), Error> {
        let filter = self.make_filter(&config.command)?;
        if filter.is_empty() {
            log::warn!("pre-filter({}) not valid", name);
        } else {
            self.filters.push(filter);
        }
        Ok(())
    }

    pub fn add_service_filter(
        &mut self,
        config: &FilterConfig,
    ) -> Result<(), Error> {
        let filter = self.make_filter(&config.command)?;
        if filter.is_empty() {
            log::warn!("service-filter not valid");
        } else {
            self.filters.push(filter);
        }
        Ok(())
    }

    pub fn add_decode_filter(
        &mut self,
        config: &FilterConfig
    ) -> Result<(), Error> {
        let filter = self.make_filter(&config.command)?;
        if filter.is_empty() {
            log::warn!("decode-filter not valid");
        } else {
            self.filters.push(filter);
        }
        Ok(())
    }

    pub fn add_program_filter(
        &mut self,
        config: &FilterConfig,
    ) -> Result<(), Error> {
        let filter = self.make_filter(&config.command)?;
        if filter.is_empty() {
            log::warn!("program-filter not valid");
        } else {
            self.filters.push(filter);
        }
        Ok(())
    }

    pub fn add_post_filters(
        &mut self,
        post_filters: &HashMap<String, PostFilterConfig>,
        names: &Vec<String>
    ) -> Result<(), Error> {
        for name in names.iter() {
            if post_filters.contains_key(name) {
                self.add_post_filter(&post_filters[name], name)?;
            } else {
                log::warn!("No such post-filter: {}", name);
            }
        }
        Ok(())
    }

    pub fn add_post_filter(
        &mut self,
        config: &PostFilterConfig,
        name: &str,
    ) -> Result<(), Error> {
        let filter = self.make_filter(&config.command)?;
        if filter.is_empty() {
            log::warn!("post-filter({}) not valid", name);
        } else {
            self.filters.push(filter);
            if let Some(content_type) = config.content_type.as_ref() {
                self.content_type = content_type.clone();
            }
        }
        Ok(())
    }

    pub fn make_filter(&self, command: &str) -> Result<String, Error> {
        let template = mustache::compile_str(command)?;
        Ok(template.render_data_to_string(&self.data)?.trim().to_string())
    }
}
