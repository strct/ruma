//! Details of the `ruma_api` procedural macro.

use proc_macro2::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    parse::{Parse, ParseStream},
    Field, Token, Type,
};

pub(crate) mod attribute;
pub(crate) mod metadata;
pub(crate) mod request;
pub(crate) mod response;

use self::{metadata::Metadata, request::Request, response::Response};
use crate::util;

/// Removes `serde` attributes from struct fields.
pub fn strip_serde_attrs(field: &Field) -> Field {
    let mut field = field.clone();
    field.attrs.retain(|attr| !attr.path.is_ident("serde"));
    field
}

/// The result of processing the `ruma_api` macro, ready for output back to source code.
pub struct Api {
    /// The `metadata` section of the macro.
    metadata: Metadata,

    /// The `request` section of the macro.
    request: Request,

    /// The `response` section of the macro.
    response: Response,

    /// The `error` section of the macro.
    error_ty: TokenStream,
}

impl Parse for Api {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let ruma_api = util::import_ruma_api();

        let metadata: Metadata = input.parse()?;
        let request: Request = input.parse()?;
        let response: Response = input.parse()?;
        let error_ty = match input.parse::<ErrorType>() {
            Ok(err) => err.ty.to_token_stream(),
            Err(_) => quote! { #ruma_api::error::Void },
        };

        let newtype_body_field = request.newtype_body_field();
        if metadata.method == "GET" && (request.has_body_fields() || newtype_body_field.is_some()) {
            let mut combined_error: Option<syn::Error> = None;
            let mut add_error = |field| {
                let error = syn::Error::new_spanned(field, "GET endpoints can't have body fields");
                if let Some(combined_error_ref) = &mut combined_error {
                    combined_error_ref.combine(error);
                } else {
                    combined_error = Some(error);
                }
            };

            for field in request.body_fields() {
                add_error(field);
            }

            if let Some(field) = newtype_body_field {
                add_error(field);
            }

            return Err(combined_error.unwrap());
        }

        Ok(Self { metadata, request, response, error_ty })
    }
}

pub fn expand_all(api: Api) -> syn::Result<TokenStream> {
    // Guarantee `ruma_api` is available and named something we can refer to.
    let ruma_api_import = util::import_ruma_api();

    let description = &api.metadata.description;
    let method = &api.metadata.method;
    // We don't (currently) use this literal as a literal in the generated code. Instead we just
    // put it into doc comments, for which the span information is irrelevant. So we can work
    // with only the literal's value from here on.
    let name = &api.metadata.name.value();
    let path = &api.metadata.path;
    let rate_limited = &api.metadata.rate_limited;
    let authentication = &api.metadata.authentication;

    let request_type = &api.request;
    let response_type = &api.response;

    let incoming_request_type =
        if api.request.contains_lifetimes() { quote!(IncomingRequest) } else { quote!(Request) };

    let extract_request_path = if api.request.has_path_fields() {
        quote! {
            let path_segments: ::std::vec::Vec<&::std::primitive::str> =
                request.uri().path()[1..].split('/').collect();
        }
    } else {
        TokenStream::new()
    };

    let (request_path_string, parse_request_path) =
        util::request_path_string_and_parse(&api.request, &api.metadata, &ruma_api_import);

    let request_query_string = util::build_query_string(&api.request, &ruma_api_import);

    let extract_request_query = util::extract_request_query(&api.request, &ruma_api_import);

    let parse_request_query = if let Some(field) = api.request.query_map_field() {
        let field_name = field.ident.as_ref().expect("expected field to have an identifier");

        quote! {
            #field_name: request_query,
        }
    } else {
        api.request.request_init_query_fields()
    };

    let mut header_kvs = api.request.append_header_kvs();
    if authentication == "AccessToken" {
        header_kvs.extend(quote! {
            req_builder = req_builder.header(
                #ruma_api_import::exports::http::header::AUTHORIZATION,
                #ruma_api_import::exports::http::header::HeaderValue::from_str(
                    &::std::format!(
                        "Bearer {}",
                        access_token.ok_or(
                            #ruma_api_import::error::IntoHttpError::NeedsAuthentication
                        )?
                    )
                )?
            );
        });
    }

    let extract_request_headers = if api.request.has_header_fields() {
        quote! {
            let headers = request.headers();
        }
    } else {
        TokenStream::new()
    };

    let extract_request_body =
        if api.request.has_body_fields() || api.request.newtype_body_field().is_some() {
            let body_lifetimes = if api.request.has_body_lifetimes() {
                // duplicate the anonymous lifetime as many times as needed
                let lifetimes =
                    std::iter::repeat(quote! { '_ }).take(api.request.body_lifetime_count());
                quote! { < #( #lifetimes, )* >}
            } else {
                TokenStream::new()
            };
            quote! {
                let request_body: <
                    RequestBody #body_lifetimes
                    as #ruma_api_import::exports::ruma_serde::Outgoing
                >::Incoming = {
                    // If the request body is completely empty, pretend it is an empty JSON object
                    // instead. This allows requests with only optional body parameters to be
                    // deserialized in that case.
                    let json = match request.body().as_slice() {
                        b"" => b"{}",
                        body => body,
                    };

                    #ruma_api_import::try_deserialize!(
                        request,
                        #ruma_api_import::exports::serde_json::from_slice(json)
                    )
                };
            }
        } else {
            TokenStream::new()
        };

    let parse_request_headers = if api.request.has_header_fields() {
        api.request.parse_headers_from_request()
    } else {
        TokenStream::new()
    };

    let request_body = util::build_request_body(&api.request, &ruma_api_import);

    let parse_request_body = util::parse_request_body(&api.request);

    let extract_response_headers = if api.response.has_header_fields() {
        quote! {
            let mut headers = response.headers().clone();
        }
    } else {
        TokenStream::new()
    };

    let typed_response_body_decl =
        if api.response.has_body_fields() || api.response.newtype_body_field().is_some() {
            quote! {
                let response_body: <
                    ResponseBody
                    as #ruma_api_import::exports::ruma_serde::Outgoing
                >::Incoming = {
                    // If the reponse body is completely empty, pretend it is an empty JSON object
                    // instead. This allows reponses with only optional body parameters to be
                    // deserialized in that case.
                    let json = match response.body().as_slice() {
                        b"" => b"{}",
                        body => body,
                    };

                    #ruma_api_import::try_deserialize!(
                        response,
                        #ruma_api_import::exports::serde_json::from_slice(json),
                    )
                };
            }
        } else {
            TokenStream::new()
        };

    let response_init_fields = api.response.init_fields();

    let serialize_response_headers = api.response.apply_header_fields();

    let body = api.response.to_body();

    let metadata_doc = format!("Metadata for the `{}` API endpoint.", name);
    let request_doc =
        format!("Data for a request to the `{}` API endpoint.\n\n{}", name, description.value());
    let response_doc = format!("Data in the response from the `{}` API endpoint.", name);

    let error = &api.error_ty;

    let request_lifetimes = api.request.combine_lifetimes();

    let non_auth_endpoint_impls = if authentication != "None" {
        TokenStream::new()
    } else {
        quote! {
            impl #request_lifetimes #ruma_api_import::OutgoingNonAuthRequest
                for Request #request_lifetimes
            {}

            impl #ruma_api_import::IncomingNonAuthRequest for #incoming_request_type {}
        }
    };

    Ok(quote! {
        #[doc = #request_doc]
        #request_type

        impl ::std::convert::TryFrom<#ruma_api_import::exports::http::Request<Vec<u8>>>
            for #incoming_request_type
        {
            type Error = #ruma_api_import::error::FromHttpRequestError;

            #[allow(unused_variables)]
            fn try_from(
                request: #ruma_api_import::exports::http::Request<Vec<u8>>
            ) -> ::std::result::Result<Self, Self::Error> {
                #extract_request_path
                #extract_request_query
                #extract_request_headers
                #extract_request_body

                Ok(Self {
                    #parse_request_path
                    #parse_request_query
                    #parse_request_headers
                    #parse_request_body
                })
            }
        }

        #[doc = #response_doc]
        #response_type

        impl ::std::convert::TryFrom<Response>
            for #ruma_api_import::exports::http::Response<Vec<u8>>
        {
            type Error = #ruma_api_import::error::IntoHttpError;

            #[allow(unused_variables)]
            fn try_from(response: Response) -> ::std::result::Result<Self, Self::Error> {
                let mut resp_builder = #ruma_api_import::exports::http::Response::builder()
                    .header(
                        #ruma_api_import::exports::http::header::CONTENT_TYPE,
                        "application/json",
                    );

                let mut headers = resp_builder
                    .headers_mut()
                    .expect("`http::ResponseBuilder` is in unusable state");
                #serialize_response_headers

                // This cannot fail because we parse each header value
                // checking for errors as each value is inserted and
                // we only allow keys from the `http::header` module.
                let response = resp_builder.body(#body).unwrap();
                Ok(response)
            }
        }

        impl ::std::convert::TryFrom<#ruma_api_import::exports::http::Response<Vec<u8>>>
            for Response
        {
            type Error = #ruma_api_import::error::FromHttpResponseError<#error>;

            #[allow(unused_variables)]
            fn try_from(
                response: #ruma_api_import::exports::http::Response<Vec<u8>>,
            ) -> ::std::result::Result<Self, Self::Error> {
                if response.status().as_u16() < 400 {
                    #extract_response_headers

                    #typed_response_body_decl

                    Ok(Self {
                        #response_init_fields
                    })
                } else {
                    match <#error as #ruma_api_import::EndpointError>::try_from_response(response) {
                        Ok(err) => Err(#ruma_api_import::error::ServerError::Known(err).into()),
                        Err(response_err) => {
                            Err(#ruma_api_import::error::ServerError::Unknown(response_err).into())
                        }
                    }
                }
            }
        }

        #[doc = #metadata_doc]
        pub const METADATA: #ruma_api_import::Metadata = #ruma_api_import::Metadata {
            description: #description,
            method: #ruma_api_import::exports::http::Method::#method,
            name: #name,
            path: #path,
            rate_limited: #rate_limited,
            authentication: #ruma_api_import::AuthScheme::#authentication,
        };

        impl #request_lifetimes #ruma_api_import::OutgoingRequest
            for Request #request_lifetimes
        {
            type EndpointError = #error;
            type IncomingResponse =
                <Response as #ruma_api_import::exports::ruma_serde::Outgoing>::Incoming;

            #[doc = #metadata_doc]
            const METADATA: #ruma_api_import::Metadata = self::METADATA;

            #[allow(unused_mut, unused_variables)]
            fn try_into_http_request(
                self,
                base_url: &::std::primitive::str,
                access_token: ::std::option::Option<&str>,
            ) -> ::std::result::Result<
                #ruma_api_import::exports::http::Request<Vec<u8>>,
                #ruma_api_import::error::IntoHttpError,
            > {
                let metadata = self::METADATA;

                let mut req_builder = #ruma_api_import::exports::http::Request::builder()
                    .method(#ruma_api_import::exports::http::Method::#method)
                    .uri(::std::format!(
                        "{}{}{}",
                        // FIXME: Once MSRV is >= 1.45.0, switch to
                        // base_url.strip_suffix('/').unwrap_or(base_url),
                        match base_url.as_bytes().last() {
                            Some(b'/') => &base_url[..base_url.len() - 1],
                            _ => base_url,
                        },
                        #request_path_string,
                        #request_query_string,
                    ));

                #header_kvs

                let http_request = req_builder.body(#request_body)?;

                Ok(http_request)
            }
        }

        impl #ruma_api_import::IncomingRequest for #incoming_request_type {
            type EndpointError = #error;
            type OutgoingResponse = Response;

            #[doc = #metadata_doc]
            const METADATA: #ruma_api_import::Metadata = self::METADATA;
        }

        #non_auth_endpoint_impls
    })
}

mod kw {
    syn::custom_keyword!(error);
}

pub struct ErrorType {
    pub error_kw: kw::error,
    pub ty: Type,
}

impl Parse for ErrorType {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let error_kw = input.parse::<kw::error>()?;
        input.parse::<Token![:]>()?;
        let ty = input.parse()?;

        Ok(Self { error_kw, ty })
    }
}
