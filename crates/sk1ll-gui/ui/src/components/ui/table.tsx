"use client"

import * as React from "react"
import { cn } from "@/lib/utils"

function Table({ className, stickyFirstColumn = false, ...props }: React.ComponentProps<"table"> & { stickyFirstColumn?: boolean }) {
  const frame = React.useRef<HTMLDivElement>(null)
  const scroll = React.useRef<HTMLDivElement>(null)
  const label = props["aria-label"]
  const update = React.useCallback(() => {
    const container = scroll.current, outer = frame.current
    if (!container || !outer) return
    outer.dataset.overflowStart = String(container.scrollLeft > 1)
    outer.dataset.overflowEnd = String(container.scrollLeft + container.clientWidth < container.scrollWidth - 1)
    const width = stickyFirstColumn ? container.querySelector("th")?.getBoundingClientRect().width ?? 0 : 0
    outer.style.setProperty("--sticky-column-width", `${width}px`)
  }, [stickyFirstColumn])
  React.useEffect(() => {
    update()
    if (typeof ResizeObserver === "undefined") return
    const observer = new ResizeObserver(update)
    if (scroll.current) observer.observe(scroll.current)
    const table = scroll.current?.querySelector("table")
    if (table) observer.observe(table)
    const first = table?.querySelector("th")
    if (stickyFirstColumn && first) observer.observe(first)
    return () => observer.disconnect()
  }, [update, stickyFirstColumn, props.children])
  return (
    <div ref={frame} data-slot="table-frame" data-sticky-first={stickyFirstColumn || undefined} className="relative min-w-0">
      <div
        ref={scroll}
        data-slot="table-container"
        role={label ? "region" : undefined}
        aria-label={label}
        tabIndex={label ? 0 : undefined}
        onScroll={update}
        onFocusCapture={event => {
          const container = scroll.current, target = event.target as HTMLElement
          if (!stickyFirstColumn || !container || target === container || target.closest<HTMLTableCellElement>("td, th")?.cellIndex === 0) return
          const bounds = container.getBoundingClientRect()
          const left = bounds.left + Number.parseFloat(frame.current?.style.getPropertyValue("--sticky-column-width") || "0") + 8
          const focused = target.getBoundingClientRect()
          if (focused.left < left) container.scrollLeft -= left - focused.left
          else if (focused.right > bounds.right - 8) container.scrollLeft += focused.right - bounds.right + 8
        }}
        className={cn("relative w-full overflow-x-auto outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset", stickyFirstColumn && "[scroll-padding-inline-start:var(--sticky-column-width)]")}
      >
        <table data-slot="table" className={cn("w-full caption-bottom text-sm", className)} {...props} />
      </div>
    </div>
  )
}

function TableHeader({ className, ...props }: React.ComponentProps<"thead">) {
  return (
    <thead
      data-slot="table-header"
      className={cn("bg-muted text-xs text-muted-foreground [&_tr]:border-b [&_tr]:hover:bg-transparent", className)}
      {...props}
    />
  )
}

function TableBody({ className, ...props }: React.ComponentProps<"tbody">) {
  return (
    <tbody
      data-slot="table-body"
      className={cn("[&_tr:last-child]:border-0", className)}
      {...props}
    />
  )
}

function TableFooter({ className, ...props }: React.ComponentProps<"tfoot">) {
  return (
    <tfoot
      data-slot="table-footer"
      className={cn(
        "border-t bg-muted/50 font-medium [&>tr]:last:border-b-0",
        className
      )}
      {...props}
    />
  )
}

function TableRow({ className, ...props }: React.ComponentProps<"tr">) {
  return (
    <tr
      data-slot="table-row"
      className={cn(
        "border-b transition-colors hover:bg-muted/50 has-aria-expanded:bg-muted/50 data-[state=selected]:bg-muted",
        className
      )}
      {...props}
    />
  )
}

function TableHead({ className, ...props }: React.ComponentProps<"th">) {
  return (
    <th
      data-slot="table-head"
      className={cn(
        "h-9 px-2 text-left align-middle font-medium whitespace-nowrap first:pl-3.5 last:pr-3.5 [&:has([role=checkbox])]:pr-0",
        className
      )}
      {...props}
    />
  )
}

function TableCell({ className, ...props }: React.ComponentProps<"td">) {
  return (
    <td
      data-slot="table-cell"
      className={cn(
        "p-2 align-middle whitespace-nowrap first:pl-3.5 last:pr-3.5 [&:has([role=checkbox])]:pr-0",
        className
      )}
      {...props}
    />
  )
}

function TableCaption({
  className,
  ...props
}: React.ComponentProps<"caption">) {
  return (
    <caption
      data-slot="table-caption"
      className={cn("mt-4 text-sm text-muted-foreground", className)}
      {...props}
    />
  )
}

export {
  Table,
  TableHeader,
  TableBody,
  TableFooter,
  TableHead,
  TableRow,
  TableCell,
  TableCaption,
}
