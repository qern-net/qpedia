<script lang="ts">
  import type { SourceStatus } from '$lib/api';
  let { status }: { status: SourceStatus } = $props();
</script>

<span class="chip chip-{status}">{status.replace(/_/g, ' ')}</span>
