import { UsagePoolDetailClient } from "./UsagePoolDetailClient";

export const dynamic = "force-dynamic";

export default async function UsagePoolDetailPage({
  params,
  searchParams,
}: {
  params: Promise<{ id: string }>;
  searchParams: Promise<{ setup?: string | string[] }>;
}) {
  const [{ id }, query] = await Promise.all([params, searchParams]);
  const setup = Array.isArray(query.setup) ? query.setup[0] : query.setup;
  return <UsagePoolDetailClient poolId={id} setup={setup} />;
}
