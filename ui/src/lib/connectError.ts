import { ConnectError, Code } from "@connectrpc/connect";
import { NextResponse } from "next/server";

/**
 * Translate a [`ConnectError`] (or unknown) into the HTTP error envelope
 * this UI's `/api/admin/*` routes have always returned. Lets us migrate
 * route handlers to Connect without breaking client error handling.
 */
export function connectErrorResponse(e: unknown): NextResponse {
  if (e instanceof ConnectError) {
    return NextResponse.json(
      {
        error: {
          message: e.rawMessage,
          error_type: errorTypeFor(e.code),
          param: null,
          code: null,
        },
      },
      { status: httpStatusFor(e.code) },
    );
  }
  return NextResponse.json(
    {
      error: {
        message: e instanceof Error ? e.message : String(e),
        error_type: "internal_error",
        param: null,
        code: null,
      },
    },
    { status: 500 },
  );
}

function httpStatusFor(code: Code): number {
  switch (code) {
    case Code.AlreadyExists:
      return 409;
    case Code.NotFound:
      return 404;
    case Code.InvalidArgument:
      return 400;
    case Code.FailedPrecondition:
      return 403;
    case Code.Unauthenticated:
      return 401;
    case Code.PermissionDenied:
      return 403;
    case Code.ResourceExhausted:
      return 429;
    case Code.Unimplemented:
      return 501;
    default:
      return 500;
  }
}

function errorTypeFor(code: Code): string {
  switch (code) {
    case Code.AlreadyExists:
      return "already_exists";
    case Code.NotFound:
      return "not_found";
    case Code.InvalidArgument:
      return "invalid_request";
    case Code.FailedPrecondition:
      return "configuration_error";
    case Code.Unauthenticated:
      return "authentication_error";
    case Code.PermissionDenied:
      return "permission_error";
    case Code.ResourceExhausted:
      return "rate_limit_error";
    default:
      return "internal_error";
  }
}
