use crate::{errors, structured_errors::StructuredDiagnostic};
use rustc_errors::{codes::*, DiagnosticBuilder};
use rustc_middle::ty::{Ty, TypeVisitableExt};
use rustc_session::Session;
use rustc_span::Span;

pub struct SizedUnsizedCast<'tcx> {
    pub sess: &'tcx Session,
    pub span: Span,
    pub expr_ty: Ty<'tcx>,
    pub cast_ty: String,
}

impl<'tcx> StructuredDiagnostic<'tcx> for SizedUnsizedCast<'tcx> {
    fn session(&self) -> &Session {
        self.sess
    }

    fn code(&self) -> ErrCode {
        E0607
    }

    fn diagnostic_common(&self) -> DiagnosticBuilder<'tcx> {
        let mut err = self.sess.dcx().create_err(errors::CastThinPointerToFatPointer {
            span: self.span,
            expr_ty: self.expr_ty,
            cast_ty: self.cast_ty.to_owned(),
        });

        if self.expr_ty.references_error() {
            err.downgrade_to_delayed_bug();
        }

        err
    }

    fn diagnostic_extended(&self, mut err: DiagnosticBuilder<'tcx>) -> DiagnosticBuilder<'tcx> {
        err.help(
            "Thin pointers are \"simple\" pointers: they are purely a reference to a
memory address.

Fat pointers are pointers referencing \"Dynamically Sized Types\" (also
called DST). DST don't have a statically known size, therefore they can
only exist behind some kind of pointers that contain additional
information. Slices and trait objects are DSTs. In the case of slices,
the additional information the fat pointer holds is their size.

To fix this error, don't try to cast directly between thin and fat
pointers.

For more information about casts, take a look at The Book:
https://doc.rust-lang.org/reference/expressions/operator-expr.html#type-cast-expressions",
        );
        err
    }
}
