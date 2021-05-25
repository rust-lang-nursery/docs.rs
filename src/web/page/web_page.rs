use super::TemplateData;
use crate::ctry;
use crate::web::csp::Csp;
use iron::{headers::ContentType, response::Response, status::Status, IronResult, Request};
use serde::Serialize;
use std::borrow::Cow;
use tera::Context;

/// When making using a custom status, use a closure that coerces to a `fn(&Self) -> Status`
#[macro_export]
macro_rules! impl_webpage {
    ($page:ty = $template:literal $(, status = $status:expr)? $(, content_type = $content_type:expr)? $(,)?) => {
        $crate::impl_webpage!($page = |_| ::std::borrow::Cow::Borrowed($template) $(, status = $status)? $(, content_type = $content_type)?);
    };

    ($page:ty = $template:expr $(, status = $status:expr)? $(, content_type = $content_type:expr)? $(,)?) => {
        impl $crate::web::page::WebPage for $page {
            fn template(&self) -> ::std::borrow::Cow<'static, str> {
                let template: fn(&Self) -> ::std::borrow::Cow<'static, str> = $template;
                template(self)
            }

            $(
                fn get_status(&self) -> ::iron::status::Status {
                    let status: fn(&Self) -> ::iron::status::Status = $status;
                    (status)(self)
                }
            )?

            $(
                fn content_type() -> ::iron::headers::ContentType {
                    $content_type
                }
            )?
        }
    };
}

#[derive(Serialize)]
struct TemplateContext<'a, T> {
    csp_nonce: &'a str,
    #[serde(flatten)]
    page: &'a T,
}

/// The central trait that rendering pages revolves around, it handles selecting and rendering the template
pub trait WebPage: Serialize + Sized {
    /// Turn the current instance into a `Response`, ready to be served
    // TODO: We could cache similar pages using the `&Context`
    fn into_response(self, req: &Request) -> IronResult<Response> {
        let csp_nonce = req
            .extensions
            .get::<Csp>()
            .expect("missing CSP from the request extensions")
            .nonce();

        let ctx = Context::from_serialize(&TemplateContext {
            csp_nonce,
            page: &self,
        })
        .unwrap();
        let rendered = ctry!(
            req,
            req.extensions
                .get::<TemplateData>()
                .expect("missing TemplateData from the request extensions")
                .templates
                .load()
                .render(&self.template(), &ctx)
        );

        let mut response = Response::with((self.get_status(), rendered));
        response.headers.set(Self::content_type());

        Ok(response)
    }

    /// The name of the template to be rendered
    fn template(&self) -> Cow<'static, str>;

    /// Gets the status of the request, defaults to `Ok`
    fn get_status(&self) -> Status {
        Status::Ok
    }

    /// The content type that the template should be served with, defaults to html
    fn content_type() -> ContentType {
        ContentType::html()
    }
}
