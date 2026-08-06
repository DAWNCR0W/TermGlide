//! Bounded Chrome DevTools Protocol transport.

mod cdp;

pub use cdp::{
    CdpAccessibilityNode, CdpAccessibilityState, CdpAccessibilityTree, CdpAccessibilityValue,
    CdpBrowserVersion, CdpError, CdpEvaluation, CdpEvent, CdpLimits, CdpLoadEvent, CdpMouseButton,
    CdpMouseEvent, CdpMouseEventKind, CdpNavigation, CdpProtocolError, CdpReply, CdpScreenshot,
    CdpSession, CdpTargetSession,
};
