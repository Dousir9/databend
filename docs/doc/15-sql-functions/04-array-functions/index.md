---
title: 'Array Functions'
---

| Function                             | Description                                                                                  | Example                               | Result    |
|--------------------------------------|----------------------------------------------------------------------------------------------|---------------------------------------|-----------|
| **GET(array, index)**                | Returns an element from the array by index (1-based)                                         | **GET([1, 2], 2)**                    | 2         |
| **LENGTH(array)**                    | Returns the length of the array                                                              | **LENGTH([1, 2])**                    | 2         |
| **ARRAY_CONCAT(array1, array2)**     | Concats two arrays                                                                           | **ARRAY_CONCAT([1, 2], [3, 4]**       | [1,2,3,4] |
| **ARRAY_CONTAINS(array, item)**      | Checks if the array contains a specific element                                              | **ARRAY_CONTAINS([1, 2], 1)**         | 1         |
| **ARRAY_INDEXOF(array, item)**       | Returns the index(1-based) of an element if the array contains the element                   | **ARRAY_INDEXOF([1, 2, 9], 9)**       | 3         |
| **ARRAY_SLICE(array, start[, end])** | Extracts a slice from the array by index (1-based)                                           | **ARRAY_SLICE([1, 21, 32, 4], 2, 3)** | [21,32]   |
| **ARRAY_SORT(array)**                | Sorts elements in the array in ascending order                                               | **ARRAY_SORT([1, 4, 3, 2])**          | [1,2,3,4] |
| **ARRAY_<aggr\>(array)**             | Aggregates elements in the array with an aggregate function (sum, count, avg, min, max, any) | **ARRAY_SUM([1, 2, 3, 4]**            | 10        |
| **ARRAY_UNIQUE(array)**              | Counts unique elements in the array (except NULL)                                            | **ARRAY_UNIQUE([1, 2, 3, 3, 4])**     | 4         |
| **ARRAY_DISTINCT(array)**            | Removes all duplicates and NULLs from the array without preserving the original order        | **ARRAY_DISTINCT([1, 2, 2, 4])**      | [1,2,4]   |
| **ARRAY_PREPEND(item, array)**       | Prepends an element to the array                                                             | **ARRAY_PREPEND(1, [3, 4])**          | [1,3,4]   |
| **ARRAY_APPEND(array, item)**        | Appends an element to the array                                                              | **ARRAY_APPEND([3, 4], 5)**           | [3,4,5]   |
| **ARRAY_REMOVE_FIRST(array)**        | Removes the first element from the array                                                     | **ARRAY_REMOVE_FIRST([1, 2, 3])**     | [2,3]     |
| **ARRAY_REMOVE_LAST(array)**         | Removes the last element from the array                                                      | **ARRAY_REMOVE_LAST([1, 2, 3])**      | [1,2]     |

:::note
**ARRAY_SORT(array)** can accept two optional parameters, `order` and `nullposition`, which can be specified through the syntax **ARRAY_SORT(array, order, nullposition)**.
   - `order` specifies the sorting order as either ascending (ASC) or descending (DESC). Defaults to ASC.
   - `nullposition` determines the position of NULL values in the sorting result, at the beginning (NULLS FIRST) or at the end (NULLS LAST) of the sorting output. Defaults to NULLS FIRST.
:::
