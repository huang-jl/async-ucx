use super::*;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;

mod rma;
mod stream;
mod tag;

pub use self::rma::*;
pub use self::stream::*;
pub use self::tag::*;

#[derive(Debug)]
pub struct Endpoint {
    pub(super) handle: ucp_ep_h,
    pub(super) worker: Rc<Worker>,
}

impl Endpoint {
    pub(super) fn new(worker: &Rc<Worker>, addr: SocketAddr) -> Rc<Self> {
        let sockaddr = os_socketaddr::OsSocketAddr::from(addr);
        let params = ucp_ep_params {
            field_mask: (ucp_ep_params_field::UCP_EP_PARAM_FIELD_FLAGS
                | ucp_ep_params_field::UCP_EP_PARAM_FIELD_SOCK_ADDR)
                .0 as u64,
            flags: ucp_ep_params_flags_field::UCP_EP_PARAMS_FLAGS_CLIENT_SERVER.0,
            sockaddr: ucs_sock_addr {
                addr: sockaddr.as_ptr() as _,
                addrlen: sockaddr.len(),
            },
            // set NONE to enable TCP
            // ref: https://github.com/rapidsai/ucx-py/issues/194#issuecomment-535726896
            err_mode: ucp_err_handling_mode_t::UCP_ERR_HANDLING_MODE_NONE,
            err_handler: ucp_err_handler {
                cb: None,
                arg: null_mut(),
            },
            user_data: null_mut(),
            address: null_mut(),
            conn_request: null_mut(),
        };
        let mut handle = MaybeUninit::uninit();
        let status = unsafe { ucp_ep_create(worker.handle, &params, handle.as_mut_ptr()) };
        assert_eq!(status, ucs_status_t::UCS_OK);
        let handle = unsafe { handle.assume_init() };
        trace!("create endpoint={:?}", handle);
        Rc::new(Endpoint {
            handle,
            worker: worker.clone(),
        })
    }

    pub fn print_to_stderr(&self) {
        unsafe { ucp_ep_print_info(self.handle, stderr) };
    }

    /// This routine flushes all outstanding AMO and RMA communications on the endpoint.
    pub fn flush(&self) {
        let status = unsafe { ucp_ep_flush(self.handle) };
        assert_eq!(status, ucs_status_t::UCS_OK);
    }

    /// This routine flushes all outstanding AMO and RMA communications on the endpoint.
    pub fn flush_begin(&self) {
        unsafe extern "C" fn callback(request: *mut c_void, _status: ucs_status_t) {
            ucp_request_free(request);
        }
        unsafe { ucp_ep_flush_nb(self.handle, 0, Some(callback)) };
    }

    pub fn worker(&self) -> &Rc<Worker> {
        &self.worker
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        trace!("destroy endpoint={:?}", self.handle);
        unsafe { ucp_ep_destroy(self.handle) }
    }
}

/// A handle to the request returned from async IO functions.
struct RequestHandle<T> {
    ptr: ucs_status_ptr_t,
    poll_fn: fn(ucs_status_ptr_t) -> Poll<T>,
}

impl<T> Future for RequestHandle<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        if let ret @ Poll::Ready(_) = (self.poll_fn)(self.ptr) {
            return ret;
        }
        let request = unsafe { &mut *(self.ptr as *mut Request) };
        request.waker.register(cx.waker());
        (self.poll_fn)(self.ptr)
    }
}

impl<T> Drop for RequestHandle<T> {
    fn drop(&mut self) {
        trace!("request free: {:?}", self.ptr);
        unsafe { ucp_request_free(self.ptr as _) };
    }
}

fn poll_normal(ptr: ucs_status_ptr_t) -> Poll<()> {
    unsafe {
        let status = ucp_request_check_status(ptr as _);
        if status == ucs_status_t::UCS_INPROGRESS {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}
